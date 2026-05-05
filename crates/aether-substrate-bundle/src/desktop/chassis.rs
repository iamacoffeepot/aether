//! Desktop chassis: `DesktopChassis` (ADR-0035 / ADR-0071), the
//! chassis-registered control-plane handler that owns the three
//! desktop-only kinds (`capture_frame`, `set_window_mode`,
//! `platform_info`), and the [`DesktopChassis::build`] entry point
//! (ADR-0071 phase 3) that assembles the substrate + driver into a
//! [`BuiltChassis`] for `main()` to drive.
//!
//! The control handler runs on a scheduler worker (same thread as
//! every other sink handler), so the operations that need winit/wgpu
//! access forward to the event-loop thread via
//! `EventLoopProxy<UserEvent>`. `capture_frame` orchestrates its own
//! mail envelopes (pre-capture bundle push + after-capture bundle
//! resolution) and routes through `CaptureQueue` to hand off to the
//! render thread.

use std::sync::Arc;

use aether_data::Kind;
use aether_kinds::{
    Advance, CaptureFrame, PlatformInfo, SetWindowMode, SetWindowModeResult, SetWindowTitle,
    SetWindowTitleResult, WindowMode,
};
use aether_substrate::capability::BootError;
use aether_substrate::chassis_builder::{Builder, BuiltChassis};
use aether_substrate::{
    Chassis, ChassisControlHandler, HubOutbound, Mailer, Registry, ReplyTo, SubstrateBoot,
    capabilities::{
        AudioCapability, IoCapability, LogCapability, NetCapability, RenderCapability,
        RenderConfig, audio::AudioConfig as AudioConf, io::NamespaceRoots,
        net::NetConfig as NetConf,
    },
    capture::{CaptureQueue, begin_capture_request, reply_unsupported_advance},
    control::decode_payload,
};
use winit::event_loop::{EventLoop, EventLoopProxy};

use super::driver::{DesktopDriverCapability, WORKERS, parse_window_mode_env};

/// Event the event-loop thread consumes from the desktop chassis.
/// Either a chassis-originated request for work that needs winit/wgpu
/// context (platform info, window mode, capture) or a wake-up so the
/// loop picks up a queued capture on the next redraw.
#[derive(Debug, Clone)]
pub enum UserEvent {
    /// A capture was just enqueued on `CaptureQueue`; wake the loop
    /// so `RedrawRequested` pulls and fulfils it, even under
    /// `ControlFlow::Wait` when the window is occluded.
    Capture,
    /// An MCP session asked for a `platform_info` snapshot. The
    /// event-loop thread snapshots + replies via outbound.
    PlatformInfo { reply_to: ReplyTo },
    /// An MCP session asked to switch the window mode. The event
    /// loop resolves fullscreen modes against the current monitor,
    /// applies the change, and replies with the new state.
    SetWindowMode {
        reply_to: ReplyTo,
        mode: WindowMode,
        width: Option<u32>,
        height: Option<u32>,
    },
    /// An MCP session asked to update the window title. The event
    /// loop calls `Window::set_title` and echoes the applied title
    /// back on the reply. A missing window (before `resumed`) replies
    /// with an `Err`.
    SetWindowTitle { reply_to: ReplyTo, title: String },
}

/// Build the `ChassisControlHandler` closure desktop installs on
/// `ControlPlane::chassis_handler`. Captures the handles each
/// chassis-specific kind needs: the event-loop proxy for hand-off to
/// winit/wgpu context; the capture queue for render-thread handoff;
/// the registry + queue for capture_frame's mail-bundle orchestration;
/// the outbound handle for inline error replies.
pub fn chassis_control_handler(
    proxy: EventLoopProxy<UserEvent>,
    capture_queue: CaptureQueue,
    registry: Arc<Registry>,
    queue: Arc<Mailer>,
    outbound: Arc<HubOutbound>,
) -> ChassisControlHandler {
    Arc::new(
        move |kind: aether_data::KindId, kind_name: &str, sender: ReplyTo, bytes: &[u8]| match kind
        {
            CaptureFrame::ID => {
                let proxy = proxy.clone();
                begin_capture_request(
                    &queue,
                    &capture_queue,
                    &registry,
                    &outbound,
                    sender,
                    bytes,
                    move || {
                        // `send_event` only fails if the event loop
                        // has shut down; in that case nothing listens
                        // for captures anyway, so swallow the error
                        // and let the queued capture sit until exit.
                        let _ = proxy.send_event(UserEvent::Capture);
                        Ok(())
                    },
                );
            }
            PlatformInfo::ID => {
                // Empty payload; forward the sender straight to the
                // event loop and let it snapshot + reply on its own
                // thread (winit monitor / scale-factor APIs require it).
                let _ = proxy.send_event(UserEvent::PlatformInfo { reply_to: sender });
            }
            SetWindowMode::ID => {
                handle_set_window_mode(&proxy, &outbound, sender, bytes);
            }
            SetWindowTitle::ID => {
                handle_set_window_title(&proxy, &outbound, sender, bytes);
            }
            Advance::ID => {
                reply_unsupported_advance(
                    &outbound,
                    sender,
                    "unsupported on desktop chassis — aether.test_bench.advance is \
                     test-bench-only (ADR-0067)",
                );
            }
            _ => {
                tracing::warn!(
                    target: "aether_substrate::chassis",
                    kind = %kind_name,
                    "desktop chassis has no handler for control kind — dropping",
                );
            }
        },
    )
}

/// Decode + forward to the event loop. Applying the mode requires
/// winit APIs that only live on the main thread, so this handler
/// doesn't reply inline on the happy path — the event loop does.
fn handle_set_window_mode(
    proxy: &EventLoopProxy<UserEvent>,
    outbound: &HubOutbound,
    sender: ReplyTo,
    bytes: &[u8],
) {
    let payload: SetWindowMode = match decode_payload(bytes) {
        Ok(p) => p,
        Err(error) => {
            outbound.send_reply(sender, &SetWindowModeResult::Err { error });
            return;
        }
    };
    let _ = proxy.send_event(UserEvent::SetWindowMode {
        reply_to: sender,
        mode: payload.mode,
        width: payload.width,
        height: payload.height,
    });
}

/// Decode + forward to the event loop. `Window::set_title` needs to
/// run on the main thread on every winit platform, so the same
/// event-loop proxy hand-off `set_window_mode` uses.
fn handle_set_window_title(
    proxy: &EventLoopProxy<UserEvent>,
    outbound: &HubOutbound,
    sender: ReplyTo,
    bytes: &[u8],
) {
    let payload: SetWindowTitle = match decode_payload(bytes) {
        Ok(p) => p,
        Err(error) => {
            outbound.send_reply(sender, &SetWindowTitleResult::Err { error });
            return;
        }
    };
    let _ = proxy.send_event(UserEvent::SetWindowTitle {
        reply_to: sender,
        title: payload.title,
    });
}

/// Marker type for the desktop chassis. Carries no fields — the
/// chassis instance is the [`BuiltChassis<DesktopChassis>`] returned
/// by [`Self::build`]. The unit struct exists so the chassis_builder
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
    pub hub_url: Option<String>,
    pub namespace_roots: NamespaceRoots,
    pub net: NetConf,
    pub audio: AudioConf,
    pub boot_mode: WindowMode,
    pub boot_size: Option<(u32, u32)>,
    pub boot_title: String,
}

impl DesktopEnv {
    /// Read every chassis-relevant env var into a fresh `DesktopEnv`,
    /// constructing the winit `EventLoop` + `CaptureQueue` along the
    /// way. The single env-reading edge for the desktop chassis (per
    /// issue 464). Tests bypass this by constructing `DesktopEnv`
    /// directly.
    pub fn from_env() -> wasmtime::Result<Self> {
        let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
        event_loop.set_control_flow(winit::event_loop::ControlFlow::Poll);
        let capture_queue = CaptureQueue::new();

        let hub_url = std::env::var("AETHER_HUB_URL").ok();
        let net = NetConf::from_env();
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

        Ok(DesktopEnv {
            event_loop,
            capture_queue,
            hub_url,
            namespace_roots,
            net,
            audio,
            boot_mode,
            boot_size,
            boot_title,
        })
    }
}

impl DesktopChassis {
    /// Build the desktop chassis: stand up substrate-core internals,
    /// connect to the hub if requested, compose the native passives
    /// (log, io, net, audio, render+camera) through the
    /// chassis_builder `.with()` chain, then wrap everything in a
    /// [`DesktopDriverCapability`] and hand it to the builder.
    /// Returns a [`BuiltChassis`] whose [`BuiltChassis::run`] blocks
    /// on the winit event loop.
    ///
    /// The trait method [`Chassis::build`] forwards here.
    fn build_inner(env: DesktopEnv) -> Result<BuiltChassis<DesktopChassis>, BootError> {
        let DesktopEnv {
            event_loop,
            capture_queue,
            hub_url,
            namespace_roots,
            net,
            audio,
            boot_mode,
            boot_size,
            boot_title,
        } = env;

        let boot = SubstrateBoot::builder("hello-triangle", env!("CARGO_PKG_VERSION"))
            .workers(WORKERS)
            .namespace_roots(namespace_roots)
            .chassis_handler({
                let proxy = event_loop.create_proxy();
                let cq = capture_queue.clone();
                move |ctx| {
                    Some(chassis_control_handler(
                        proxy,
                        cq,
                        Arc::clone(ctx.registry),
                        Arc::clone(ctx.queue),
                        Arc::clone(ctx.outbound),
                    ))
                }
            })
            .build()?;

        tracing::info!(
            target: "aether_substrate::boot",
            workers = WORKERS,
            "componentless boot — close window to exit; load a component via aether.control.load_component",
        );

        let boot_kinds_count = boot.boot_descriptors.len() as u32;

        // Hub connect AFTER every chassis sink is registered (issue #262).
        // Post-ADR-0070 phase 4: the hub client lives in `aether-hub`;
        // substrate-core has no hub knowledge. The free-function form
        // matches the pre-refactor `boot.connect_hub` shape; chassis
        // that prefer Builder-pipeline composition can swap in
        // `aether_hub::HubClientCapability` instead (the free fn is
        // a thin wrapper around the same path).
        let hub = crate::hub::connect_hub_client(&boot, hub_url.as_deref())?;

        let registry = Arc::clone(&boot.registry);
        let mailer = Arc::clone(&boot.queue);
        let namespace_roots = boot.namespace_roots.clone();
        // ADR-0074 §Decision 5: production chassis configures the
        // cross-class `wait_reply` aborter to broadcast
        // `SubstrateDying` before exit. Built before `boot` moves
        // into the driver.
        let aborter: Arc<dyn aether_substrate::lifecycle::FatalAborter> = Arc::new(
            aether_substrate::lifecycle::OutboundFatalAborter::new(Arc::clone(&boot.outbound)),
        );

        // PR E2: render is now a `#[actor]` cap. Extract the
        // driver-facing accumulator + GPU bundle before the cap moves
        // into the chassis builder; the dispatcher thread takes the
        // cap by value and the driver retains the Arc-shared handles.
        let render_cap = RenderCapability::new(RenderConfig::default());
        let render_handles = render_cap.handles();

        let driver = DesktopDriverCapability {
            event_loop,
            boot,
            capture_queue,
            boot_kinds_count,
            boot_mode,
            boot_size,
            boot_title,
            hub,
            render_handles,
        };

        // Boot order is declaration order — log first so other
        // capabilities' boot tracing routes through the log capture;
        // render last so it claims its mailboxes after every other
        // chassis cap.
        Builder::<DesktopChassis>::new(registry, Arc::clone(&mailer))
            .with_aborter(aborter)
            .with_actor::<LogCapability>(())
            .with_actor::<IoCapability>(namespace_roots)
            .with_actor::<NetCapability>(net)
            .with_actor::<AudioCapability>(audio)
            .with(render_cap)
            .driver(driver)
            .build()
    }
}
