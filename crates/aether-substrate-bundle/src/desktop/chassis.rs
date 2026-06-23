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

use std::error::Error as StdError;
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;

use aether_actor::Addressable;
use aether_capabilities::LifecycleCapability;
use aether_capabilities::rpc::RpcServerCapability;
use aether_capabilities::{
    AnthropicConfig, AudioCapability, CaptureBackend, ComponentHostConfig, GeminiConfig,
    HttpServerConfig, InputConfig, RenderCapability, RenderConfig, UnsupportedTestBenchCapability,
    audio::AudioConfig as AudioConf, fs::NamespaceRoots, http::HttpConfig as HttpConf,
};
use aether_kinds::BinaryManifest;
use aether_kinds::WindowMode;
use aether_substrate::chassis::builder::{Builder, BuiltChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::{Chassis, SubstrateBoot, capture::CaptureQueue};
use winit::error::EventLoopError;
use winit::event_loop::EventLoop;

use super::driver::{DesktopDriverCapability, WindowConfig};
use crate::autoload::{AutoloadComponent, autoload_mail, boot_manifest_autoload};
use crate::chassis_common::{
    ActorRingConfig, ChassisBootConfig, CommonBoot, chassis_known_keys, frame_lifecycle_config,
    maybe_with_http_server, maybe_with_rpc_server, with_common_caps,
};
use crate::cli::{CommonOverlay, DesktopCli};
use crate::hub;
use aether_substrate::config::{ConfigError, RingCapacities, validate_env};
use aether_substrate::runtime::lifecycle::FatalAborter;
use aether_substrate::runtime::lifecycle::OutboundFatalAborter;
use std::path::Path;
use winit::event_loop::ControlFlow;

/// Desktop chassis env-resolution failure (ADR-0090 §4 / issue #571).
/// Widens the historic `EventLoopError`-only return so the desktop
/// resolver can surface both the winit event-loop fault *and* a config
/// fault (an unparseable known `AETHER_*` env value). Both arms
/// `From`-convert in, and the whole enum `From`-converts into
/// `anyhow::Error` via its `StdError` impl so `main()` keeps using `?`.
#[derive(Debug)]
pub enum DesktopBootError {
    /// winit `EventLoop::build` failed.
    EventLoop(EventLoopError),
    /// A known `AETHER_*` env var (or argv overlay value) was
    /// unparseable (ADR-0090 §4 hard-error half).
    Config(ConfigError),
}

impl fmt::Display for DesktopBootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EventLoop(e) => write!(f, "desktop event loop build failed: {e}"),
            Self::Config(e) => write!(f, "desktop config resolution failed: {e}"),
        }
    }
}

impl StdError for DesktopBootError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::EventLoop(e) => Some(e),
            Self::Config(e) => Some(e),
        }
    }
}

impl From<EventLoopError> for DesktopBootError {
    fn from(e: EventLoopError) -> Self {
        Self::EventLoop(e)
    }
}

impl From<ConfigError> for DesktopBootError {
    fn from(e: ConfigError) -> Self {
        Self::Config(e)
    }
}

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
    /// The `--describe` manifest (ADR-0115, issue 1953): the chassis
    /// profile, the mailbox namespaces this binary links, and the
    /// `build.rs` provenance. The desktop chassis layers the audio /
    /// render / test-bench / lifecycle caps plus the RPC server onto the
    /// shared [`common_cap_namespaces`](crate::common_cap_namespaces)
    /// base. `--describe` prints this without opening a winit event loop —
    /// the hub can capture a desktop binary's manifest on a headless host.
    #[must_use]
    pub fn describe_manifest() -> BinaryManifest {
        let mut caps = crate::common_cap_namespaces();
        caps.extend([
            <AudioCapability as Addressable>::NAMESPACE,
            <RenderCapability as Addressable>::NAMESPACE,
            <UnsupportedTestBenchCapability as Addressable>::NAMESPACE,
            <LifecycleCapability as Addressable>::NAMESPACE,
            <RpcServerCapability as Addressable>::NAMESPACE,
        ]);
        crate::binary_manifest(Self::PROFILE, caps)
    }
}

/// Bag of resolved configs the desktop chassis takes at build time.
/// `main()` populates it from env vars (per ADR-0070's "substrate-core
/// never reads env" invariant); tests construct one directly.
///
/// `event_loop` and `capture_queue` come in pre-built so `main()`
/// owns the winit + capture handoff plumbing — winit's `EventLoop`
/// is `!Send` on macOS and is the chassis's main thread in any
/// case, which keeps construction local to `main`.
pub struct DesktopEnv {
    pub event_loop: EventLoop<UserEvent>,
    pub capture_queue: CaptureQueue,
    pub namespace_roots: NamespaceRoots,
    pub http: HttpConf,
    /// ADR-0050 `aether.anthropic` cap config (issue 1014). Resolved
    /// from `ANTHROPIC_API_KEY` + `AETHER_ANTHROPIC_*`.
    pub anthropic: AnthropicConfig,
    /// ADR-0050 `aether.gemini` cap config (issue 1015). Resolved from
    /// `GEMINI_API_KEY` + `AETHER_GEMINI_*`.
    pub gemini: GeminiConfig,
    pub audio: AudioConf,
    pub boot_mode: WindowMode,
    pub boot_size: Option<(u32, u32)>,
    pub boot_title: String,
    /// Issue 1761: optional `aether.http.server` init config (ADR-0108).
    /// `Some` iff the cap's `enabled` flag is set (`AETHER_HTTP_SERVER_ENABLED`
    /// / `--http-server-enabled`); `None` (default) skips booting
    /// `HttpServerCapability` so an unconfigured chassis binds no HTTP port.
    pub http_server: Option<HttpServerConfig>,
    /// Issue 763 P2: optional `aether.rpc.server` bind address.
    /// Populated from `AETHER_RPC_PORT`; `None` (default) skips booting
    /// `RpcServerCapability` so existing chassis behavior is unchanged.
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
    /// Force-complete deadline (ms) for a pending lifecycle advance's
    /// `Settled` (issue 1048). Resolved from
    /// `AETHER_LIFECYCLE_ADVANCE_TIMEOUT_MS` via `ChassisBootConfig`;
    /// default [`aether_capabilities::LifecycleConfig::ADVANCE_TIMEOUT_MS_DEFAULT`].
    pub lifecycle_advance_timeout_millis: u64,
    /// Components to auto-load on boot, in order. A bundled standalone build
    /// populates this so the game comes up with no hub; the normal desktop bin
    /// leaves it empty and loads components over the hub instead.
    pub autoload: Vec<AutoloadComponent>,
}

impl DesktopEnv {
    /// Read every chassis-relevant env var into a fresh `DesktopEnv`,
    /// constructing the winit `EventLoop` + `CaptureQueue` along the
    /// way. The single env-reading edge for the desktop chassis (per
    /// issue 464). Tests bypass this by constructing `DesktopEnv`
    /// directly.
    ///
    /// The fallible steps are `EventLoop::build` (winit) and the
    /// ADR-0090 §4 config validation / parse path; both ride
    /// [`DesktopBootError`] (issue #571 named the winit fault; e1
    /// widens it to carry the config fault too).
    ///
    /// # Errors
    ///
    /// Returns [`DesktopBootError::EventLoop`] when winit's event loop
    /// fails to build, or [`DesktopBootError::Config`] when a known
    /// `AETHER_*` env var holds an unparseable value.
    pub fn from_env() -> Result<Self, DesktopBootError> {
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
    pub fn from_env_with_argv(cli: DesktopCli) -> Result<Self, DesktopBootError> {
        // ADR-0090 §4 (e1): warn on any unknown AETHER_ env var.
        validate_env(&chassis_known_keys())?;
        let DesktopCli {
            common,
            audio: audio_overlay,
            window: window_overlay,
            // The bin handles `--config` / `--describe` (print + exit)
            // before this resolver runs; ignore them here.
            config: _,
            describe: _,
        } = cli;
        let CommonOverlay {
            http,
            http_server: http_server_overlay,
            fs,
            anthropic,
            gemini,
            chassis_boot: chassis_boot_overlay,
            rpc_port: cli_rpc_port,
        } = common;

        let chassis_boot =
            ChassisBootConfig::try_from_argv_then_env(chassis_boot_overlay.into_layer())?;
        let window_config = WindowConfig::from_argv_then_env(window_overlay.into_layer());

        // Boot manifest: argv wins over `AETHER_BOOT_MANIFEST` (resolved
        // through `ChassisBootConfig`). When set, the listed components'
        // wasm + config are read into the autoload list `build_inner`
        // drains into `aether.component.load`; an unreadable manifest
        // aborts boot (ADR-0090 §4) via `ConfigError`.
        let autoload = match chassis_boot.boot_manifest.clone() {
            Some(path) => boot_manifest_autoload(Path::new(&path))?,
            None => Vec::new(),
        };

        let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
        event_loop.set_control_flow(ControlFlow::Poll);
        let capture_queue = CaptureQueue::new();

        let http = HttpConf::try_from_argv_then_env(http.into_layer())?;
        let anthropic = AnthropicConfig::try_from_argv_then_env(anthropic.into_layer())?;
        let gemini = GeminiConfig::try_from_argv_then_env(gemini.into_layer())?;
        let namespace_roots = NamespaceRoots::from_argv_then_env(fs.into_layer());
        // The HTTP server is opt-in: resolve its config and boot the cap
        // only when `enabled` is set (ADR-0108 / issue 1761). Default-off,
        // so an unconfigured chassis binds no HTTP port.
        let http_server_config =
            HttpServerConfig::try_from_argv_then_env(http_server_overlay.into_layer())?;
        let http_server = http_server_config.enabled.then_some(http_server_config);
        let audio = AudioConf::try_from_argv_then_env(audio_overlay.into_layer())?;

        // Window mode and title: resolved through `WindowConfig` (argv > env >
        // default). `to_boot_mode` delegates to `parse_window_mode_env` and
        // soft-falls back to `Windowed` on a bad value; `to_boot_title` maps
        // `None` / empty to `"aether"`.
        let (boot_mode, boot_size) = window_config.to_boot_mode();
        let boot_title = window_config.to_boot_title();

        let rpc_addr = {
            use std::net::{IpAddr, Ipv4Addr};
            cli_rpc_port
                .or_else(hub::rpc_port_from_env)
                .map(|p| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), p))
        };

        let workers = chassis_boot.to_workers();
        let lifecycle_advance_timeout_millis = chassis_boot.lifecycle_advance_timeout_millis;
        // Issue 1990: resolve the per-actor ring capacities from
        // `AETHER_ACTOR_{LOG,TRACE}_RING_SIZE` (ADR-0090 §4 hard-error on
        // an unparseable known value, surfaced as `DesktopBootError::Config`).
        let ring_caps = ActorRingConfig::try_from_env()?.to_ring_capacities();

        Ok(Self {
            event_loop,
            capture_queue,
            namespace_roots,
            http,
            anthropic,
            gemini,
            audio,
            boot_mode,
            boot_size,
            boot_title,
            http_server,
            rpc_addr,
            workers,
            ring_caps,
            lifecycle_advance_timeout_millis,
            autoload,
        })
    }
}

impl DesktopChassis {
    /// Build the desktop chassis: stand up substrate-core internals,
    /// compose the native passives (log, io, http, audio, render+camera)
    /// through the `chassis_builder` `.with()` chain, then wrap everything
    /// in a [`DesktopDriverCapability`] and hand it to the builder.
    /// Returns a [`BuiltChassis`] whose [`BuiltChassis::run`] blocks
    /// on the winit event loop.
    ///
    /// The trait method [`Chassis::build`] forwards here.
    fn build_inner(env: DesktopEnv) -> Result<BuiltChassis<Self>, BootError> {
        let DesktopEnv {
            event_loop,
            capture_queue,
            namespace_roots,
            http,
            http_server,
            anthropic,
            gemini,
            audio,
            boot_mode,
            boot_size,
            boot_title,
            rpc_addr,
            workers,
            ring_caps,
            lifecycle_advance_timeout_millis,
            autoload,
        } = env;

        let boot = SubstrateBoot::builder("hello-triangle", env!("CARGO_PKG_VERSION")).build()?;

        let component_host_config = ComponentHostConfig {
            engine: Arc::clone(&boot.engine),
            linker: Arc::clone(&boot.linker),
            hub_outbound: Arc::clone(&boot.outbound),
        };
        let input_config = InputConfig::default();
        // Capture handoff lives on `RenderCapability` post-issue-603
        // Phase 2. The cap dispatcher runs `on_capture_frame`, parks
        // the request on `capture_queue`, and pokes `UserEvent::Capture`
        // so `RedrawRequested` picks it up on the next frame.
        let proxy_for_render = event_loop.create_proxy();
        let render_config = RenderConfig {
            capture_backend: Some(CaptureBackend {
                queue: capture_queue.clone(),
                wake: Arc::new(move || {
                    let _ = proxy_for_render.send_event(UserEvent::Capture);
                    Ok(())
                }),
                outbound: Arc::clone(&boot.outbound),
            }),
            // The `capture_frame` similarity check (iamacoffeepot/aether#1780)
            // reads its reference image from the same `assets` root the fs
            // cap serves, so the render cap loads it off the hot path.
            assets_dir: Some(namespace_roots.assets.clone()),
            ..RenderConfig::default()
        };

        tracing::info!(
            target: "aether_substrate::boot",
            workers_override = ?workers,
            "componentless boot — close window to exit; load a component via aether.component.load",
        );

        let registry = Arc::clone(&boot.registry);
        let mailer = Arc::clone(&boot.queue);
        // ADR-0063: production chassis configures the fatal-abort
        // aborter so a wasm guest trap exits the substrate via
        // `lifecycle::fatal_abort` instead of unwinding. Built before
        // `boot` moves into the driver.
        let aborter: Arc<dyn FatalAborter> =
            Arc::new(OutboundFatalAborter::new(Arc::clone(&boot.outbound)));

        // Issue 552 stage 2d: render is a NativeActor. The chassis
        // builder constructs the cap inside `init` (called from
        // `with_actor::<RenderCapability>(config)`); the driver pulls
        // `Arc<RenderCapability>` via `DriverCtx::actor` and clones
        // `.handles()` from there.
        let driver = DesktopDriverCapability {
            event_loop,
            boot,
            capture_queue,
            boot_mode,
            boot_size,
            boot_title,
        };

        // Boot order is declaration order — `with_common_caps` runs
        // log first so other capabilities' boot tracing routes
        // through the log capture; render last so it claims its
        // mailboxes after every other chassis cap.
        let common = CommonBoot {
            aborter,
            workers,
            ring_caps,
            input_config,
            component_host_config,
            namespace_roots,
            http,
            anthropic,
            gemini,
        };
        // ADR-0082 §11 / issues 1378 + 1489: desktop drives the shared
        // `Tick → Render → Present → Tick` frame graph, with the `Quit`
        // escape to `Shutdown` on `Present` so OS-close / ctrlc drain the
        // in-flight frame before shutting down (see the driver's
        // `CloseRequested` → `Quit` bridge and terminal-reached exit).
        let builder = with_common_caps(Builder::<Self>::new(registry, Arc::clone(&mailer)), common)
            .with_actor::<AudioCapability>(audio)
            .with_actor::<RenderCapability>(render_config)
            .with_actor::<UnsupportedTestBenchCapability>(())
            .with_actor::<LifecycleCapability>(frame_lifecycle_config(
                lifecycle_advance_timeout_millis,
            ));
        let builder = maybe_with_rpc_server(builder, rpc_addr, "aether-desktop");
        let builder = maybe_with_http_server(builder, http_server);
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
