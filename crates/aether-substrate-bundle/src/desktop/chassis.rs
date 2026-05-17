//! Desktop chassis: `DesktopChassis` (ADR-0035 / ADR-0071), the
//! `UserEvent` enum the winit event loop consumes, and the
//! [`DesktopChassis::build`] entry point that assembles the substrate
//! + driver into a [`BuiltChassis`] for `main()` to drive.
//!
//! Issue 603 retired `chassis_handler` entirely: capture goes through
//! `RenderCapability` (Phase 2), window kinds through driver-as-actor
//! on `aether.window` (Phase 3), and `platform_info` was deleted as a
//! kind (Phase 4) along with the closure-fallback that served it.
//! `UserEvent::Capture` is the lone remaining proxy event — it wakes
//! the loop so a queued `CaptureQueue` request gets pulled on the
//! next redraw, even when the window is occluded.

use std::net::SocketAddr;
use std::sync::Arc;

use aether_capabilities::rpc::{PeerKind, RpcServerCapability, RpcServerConfig};
use aether_capabilities::{
    AudioCapability, CaptureBackend, ComponentHostCapability, ComponentHostConfig, FsCapability,
    HandleCapability, HttpCapability, InputCapability, InputConfig, LogCapability,
    RenderCapability, RenderConfig, TcpCapability, UnsupportedTestBenchCapability,
    audio::AudioConfig as AudioConf, fs::NamespaceRoots, http::HttpConfig as HttpConf,
    trace::TraceObserverCapability,
};
use aether_kinds::WindowMode;
use aether_substrate::chassis::builder::{Builder, BuiltChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::{Chassis, SubstrateBoot, capture::CaptureQueue};
use winit::error::EventLoopError;
use winit::event_loop::EventLoop;

use super::driver::{DesktopDriverCapability, parse_window_mode_env};

/// Event the event-loop thread consumes from the desktop chassis.
/// Just one variant today: a wake-up so the loop picks up a queued
/// capture on the next redraw, even under `ControlFlow::Wait` when
/// the window is occluded.
#[derive(Debug, Clone)]
pub enum UserEvent {
    /// A capture was just enqueued on `CaptureQueue`; wake the loop
    /// so `RedrawRequested` pulls and fulfils it.
    Capture,
}

/// Marker type for the desktop chassis. Carries no fields — the
/// chassis instance is the [`BuiltChassis<DesktopChassis>`] returned
/// by [`Self::build`]. The unit struct exists so the `chassis_builder`
/// machinery can parameterise over a concrete chassis kind for type
/// disambiguation, and so [`Chassis::PROFILE`] has a home.
pub struct DesktopChassis;

impl Chassis for DesktopChassis {
    const PROFILE: &'static str = "desktop";
    type Driver = super::driver::DesktopDriverCapability;
    type Env = DesktopEnv;

    fn build(env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        Self::build_inner(env)
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
    pub audio: AudioConf,
    pub boot_mode: WindowMode,
    pub boot_size: Option<(u32, u32)>,
    pub boot_title: String,
    /// Issue 763 P2: optional `aether.rpc.server` bind address.
    /// Populated from `AETHER_RPC_PORT`; `None` (default) skips booting
    /// `RpcServerCapability` so existing chassis behavior is unchanged.
    pub rpc_addr: Option<SocketAddr>,
    /// Issue 745: optional worker-pool size override. Populated from
    /// `AETHER_WORKERS`; `None` keeps `PoolConfig::default()` behavior
    /// (`available_parallelism() - 1`, min 1).
    pub workers: Option<usize>,
}

impl DesktopEnv {
    /// Read every chassis-relevant env var into a fresh `DesktopEnv`,
    /// constructing the winit `EventLoop` + `CaptureQueue` along the
    /// way. The single env-reading edge for the desktop chassis (per
    /// issue 464). Tests bypass this by constructing `DesktopEnv`
    /// directly.
    ///
    /// The only fallible step is `EventLoop::build`; everything else
    /// is infallible env reads. The signature names that fault rather
    /// than the historic catch-all `wasmtime::Result` (issue #571).
    pub fn from_env() -> Result<Self, EventLoopError> {
        let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
        event_loop.set_control_flow(winit::event_loop::ControlFlow::Poll);
        let capture_queue = CaptureQueue::new();

        let http = HttpConf::from_env();
        let namespace_roots = NamespaceRoots::from_env();
        let audio = AudioConf::from_env();

        let (boot_mode, boot_size) = match std::env::var("AETHER_WINDOW_MODE") {
            Ok(s) => match parse_window_mode_env(&s) {
                Ok(parsed) => parsed,
                Err(e) => {
                    tracing::warn!(
                        target: "aether_substrate::boot",
                        value = %s,
                        error = %e,
                        "AETHER_WINDOW_MODE unparseable — falling back to Windowed",
                    );
                    (WindowMode::Windowed, None)
                }
            },
            Err(_) => (WindowMode::Windowed, None),
        };
        let boot_title =
            std::env::var("AETHER_WINDOW_TITLE").unwrap_or_else(|_| "aether".to_owned());

        // `AETHER_RPC_PORT` has no default — absent means RpcServer
        // doesn't boot. Binds `127.0.0.1`, matching the hub chassis.
        let rpc_addr = {
            use std::net::{IpAddr, Ipv4Addr};
            crate::hub::rpc_port_from_env()
                .map(|p| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), p))
        };

        let workers = parse_workers_env();

        Ok(Self {
            event_loop,
            capture_queue,
            namespace_roots,
            http,
            audio,
            boot_mode,
            boot_size,
            boot_title,
            rpc_addr,
            workers,
        })
    }
}

/// Parse `AETHER_WORKERS`. Unset → `None` (chassis falls back to
/// [`aether_substrate::scheduler::PoolConfig::default`]); positive →
/// `Some(n)`; `0` → `Some(1)` with a warn (the pool requires at least
/// one worker); unparseable → `None` with a warn. Issue 745.
fn parse_workers_env() -> Option<usize> {
    let raw = std::env::var("AETHER_WORKERS").ok()?;
    match raw.trim().parse::<usize>() {
        Ok(0) => {
            tracing::warn!(
                target: "aether_substrate::boot",
                value = %raw,
                "AETHER_WORKERS=0 — clamping to 1",
            );
            Some(1)
        }
        Ok(n) => Some(n),
        Err(e) => {
            tracing::warn!(
                target: "aether_substrate::boot",
                value = %raw,
                error = %e,
                "AETHER_WORKERS unparseable — falling back to PoolConfig::default",
            );
            None
        }
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
            audio,
            boot_mode,
            boot_size,
            boot_title,
            rpc_addr,
            workers,
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
            ..RenderConfig::default()
        };

        tracing::info!(
            target: "aether_substrate::boot",
            workers_override = ?workers,
            "componentless boot — close window to exit; load a component via aether.component.load",
        );

        let registry = Arc::clone(&boot.registry);
        let mailer = Arc::clone(&boot.queue);
        // ADR-0074 §Decision 5: production chassis configures the
        // cross-class `wait_reply` aborter so the substrate exits via
        // `lifecycle::fatal_abort` instead of unwinding. Built before
        // `boot` moves into the driver.
        let aborter: Arc<dyn aether_substrate::runtime::lifecycle::FatalAborter> = Arc::new(
            aether_substrate::runtime::lifecycle::OutboundFatalAborter::new(Arc::clone(
                &boot.outbound,
            )),
        );

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

        // Boot order is declaration order — log first so other
        // capabilities' boot tracing routes through the log capture;
        // render last so it claims its mailboxes after every other
        // chassis cap.
        let mut builder = Builder::<Self>::new(registry, Arc::clone(&mailer))
            .with_aborter(aborter)
            .with_workers(workers)
            .with_actor::<HandleCapability>(())
            .with_actor::<LogCapability>(())
            .with_actor::<TraceObserverCapability>(())
            .with_actor::<InputCapability>(input_config)
            .with_actor::<ComponentHostCapability>(component_host_config)
            .with_actor::<FsCapability>(namespace_roots)
            .with_actor::<HttpCapability>(http)
            .with_actor::<TcpCapability>(())
            .with_actor::<AudioCapability>(audio)
            .with_actor::<RenderCapability>(render_config)
            .with_actor::<UnsupportedTestBenchCapability>(());
        // Issue 763 P2: boot the RPC server only when `AETHER_RPC_PORT`
        // is set, mirroring the hub chassis. The substrate becomes an
        // RPC server peer that a hub (or any client) connects out to.
        if let Some(rpc_addr) = rpc_addr {
            builder = builder.with_actor::<RpcServerCapability>(RpcServerConfig {
                bind_addr: rpc_addr.to_string(),
                peer_kind: PeerKind::Substrate {
                    engine_name: "aether-desktop".into(),
                    engine_version: env!("CARGO_PKG_VERSION").into(),
                    kinds: vec![],
                },
            });
        }
        builder
            .with_log_drain::<LogCapability>()
            .driver(driver)
            .build()
    }
}
