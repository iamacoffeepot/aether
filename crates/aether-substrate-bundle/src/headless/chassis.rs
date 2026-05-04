//! Headless chassis-registered control-plane handler.
//!
//! Headless has no GPU and no window, so `capture_frame`,
//! `set_window_mode`, and `platform_info` have nothing to answer
//! with. Rather than letting core's `ControlPlane` warn-drop the mail
//! (which would leave the sender hanging on its await-reply slot
//! until the hub's timeout fires), this closure replies inline with
//! an explicit `Err { error: ... }` so MCP tool calls fail fast and
//! with a diagnosable message. See ADR-0035 § Consequences (neutral):
//! "A headless chassis receiving set_window_mode replies with an
//! unsupported error".

use std::sync::Arc;
use std::time::Duration;

use aether_data::{Kind, KindId};
use aether_kinds::{
    Advance, CaptureFrame, FrameStats, PlatformInfo, SetMasterGain, SetMasterGainResult,
    SetWindowMode, SetWindowTitle, Tick,
};
use aether_substrate::capability::BootError;
use aether_substrate::chassis_builder::{Builder, BuiltChassis};
use aether_substrate::{
    Chassis, ChassisControlHandler, HubOutbound, ReplyTo, SubstrateBoot,
    capabilities::{
        IoCapability, LogCapability, NetCapability, io::NamespaceRoots, net::NetConfig as NetConf,
    },
    capture::{
        reply_unsupported_advance, reply_unsupported_capture_frame,
        reply_unsupported_platform_info, reply_unsupported_window_mode,
        reply_unsupported_window_title,
    },
};

use super::driver::{HeadlessTimerCapability, WORKERS, parse_tick_hz_env};

const UNSUPPORTED: &str = "unsupported on headless chassis — no GPU or window peripherals";
const UNSUPPORTED_ADVANCE: &str =
    "unsupported on headless chassis — aether.test_bench.advance is test-bench-only (ADR-0067)";

pub fn chassis_control_handler(outbound: Arc<HubOutbound>) -> ChassisControlHandler {
    Arc::new(
        move |kind: KindId, kind_name: &str, sender: ReplyTo, _bytes: &[u8]| match kind {
            CaptureFrame::ID => {
                reply_unsupported_capture_frame(&outbound, sender, UNSUPPORTED);
            }
            SetWindowMode::ID => {
                reply_unsupported_window_mode(&outbound, sender, UNSUPPORTED);
            }
            SetWindowTitle::ID => {
                reply_unsupported_window_title(&outbound, sender, UNSUPPORTED);
            }
            Advance::ID => {
                reply_unsupported_advance(&outbound, sender, UNSUPPORTED_ADVANCE);
            }
            PlatformInfo::ID => {
                // PlatformInfoResult::Err also exists — future work
                // could return a partial Ok (OS + engine info, empty
                // GPU/monitors) once headless needs that detail.
                reply_unsupported_platform_info(&outbound, sender, UNSUPPORTED);
            }
            _ => {
                tracing::warn!(
                    target: "aether_substrate::chassis",
                    kind = %kind_name,
                    "headless chassis has no handler for control kind — dropping",
                );
            }
        },
    )
}

/// Marker type for the headless chassis. Carries no fields — the
/// chassis instance is the [`BuiltChassis<HeadlessChassis>`] returned
/// by [`Self::build`]. Same shape as [`crate::DesktopChassis`] post
/// ADR-0071 phase 3.
pub struct HeadlessChassis;

impl Chassis for HeadlessChassis {
    const PROFILE: &'static str = "headless";
    type Driver = super::driver::HeadlessTimerCapability;
    type Env = HeadlessEnv;

    fn build(env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        Self::build_inner(env)
    }
}

/// Bag of resolved configs the headless chassis takes at build time.
/// `main()` populates it from env vars (per ADR-0070's "substrate-core
/// never reads env" invariant); tests construct one directly.
pub struct HeadlessEnv {
    pub hub_url: Option<String>,
    pub namespace_roots: NamespaceRoots,
    pub net: NetConf,
    pub tick_period: Duration,
}

impl HeadlessEnv {
    /// Read every chassis-relevant env var into a fresh `HeadlessEnv`.
    /// The single env-reading edge for the headless chassis (per
    /// issue 464). Tests bypass this by constructing `HeadlessEnv`
    /// directly.
    pub fn from_env() -> Self {
        let hub_url = std::env::var("AETHER_HUB_URL").ok();
        let net = NetConf::from_env();
        let namespace_roots = NamespaceRoots::from_env();
        let tick_hz = parse_tick_hz_env();
        let tick_period = Duration::from_nanos(1_000_000_000 / u64::from(tick_hz));
        HeadlessEnv {
            hub_url,
            namespace_roots,
            net,
            tick_period,
        }
    }
}

impl HeadlessChassis {
    /// Build the headless chassis: stand up substrate-core internals,
    /// register the nop chassis sinks (render / camera / audio — they
    /// keep mailbox names resolvable so desktop-designed components
    /// loaded on headless don't warn-storm), connect the hub, compose
    /// the native passives (log, io, net) through the chassis_builder
    /// `.with()` chain, then wrap the timer in a
    /// [`HeadlessTimerCapability`] and hand it to the builder.
    /// Returns a [`BuiltChassis`] whose [`BuiltChassis::run`] blocks
    /// on the tick loop. The trait method [`Chassis::build`] forwards
    /// here.
    fn build_inner(env: HeadlessEnv) -> Result<BuiltChassis<HeadlessChassis>, BootError> {
        let HeadlessEnv {
            hub_url,
            namespace_roots,
            net,
            tick_period,
        } = env;

        let boot = SubstrateBoot::builder("headless", env!("CARGO_PKG_VERSION"))
            .workers(WORKERS)
            .namespace_roots(namespace_roots)
            .chassis_handler(|ctx| Some(chassis_control_handler(Arc::clone(ctx.outbound))))
            .build()?;

        let kind_tick = boot.registry.kind_id(Tick::NAME).expect("Tick registered");
        let kind_frame_stats = boot
            .registry
            .kind_id(FrameStats::NAME)
            .expect("FrameStats registered");

        // Silent drop for `aether.render` — desktop-designed
        // components loaded on headless emit both `DrawTriangle` and
        // (post-ADR-0074 §Decision 7) `aether.camera` at the tick
        // rate; without this sink, core's mailbox-resolution warn
        // fires every tick. The camera mailbox folded into render in
        // Phase 3, so one nop sink covers both.
        boot.registry.register_sink(
            "aether.render",
            Arc::new(
                |_kind: KindId,
                 _kind_name: &str,
                 _origin: Option<&str>,
                 _sender,
                 _bytes: &[u8],
                 _count: u32| {},
            ),
        );
        // Audio nop sink — NoteOn/NoteOff fall through silently;
        // SetMasterGain replies Err so agents fail fast rather than
        // hang on a chassis with no audio device.
        let kind_set_master_gain = boot
            .registry
            .kind_id(SetMasterGain::NAME)
            .expect("SetMasterGain registered");
        let outbound_for_audio_sink = Arc::clone(&boot.outbound);
        boot.registry.register_sink(
            "aether.audio",
            Arc::new(
                move |kind: KindId,
                      _kind_name: &str,
                      _origin: Option<&str>,
                      sender,
                      _bytes: &[u8],
                      _count: u32| {
                    if kind == kind_set_master_gain {
                        outbound_for_audio_sink.send_reply(
                            sender,
                            &SetMasterGainResult::Err {
                                error: "unsupported on headless chassis — no audio device"
                                    .to_owned(),
                            },
                        );
                    }
                },
            ),
        );

        let tick_hz = (Duration::from_secs(1).as_nanos() / tick_period.as_nanos().max(1)) as u32;
        tracing::info!(
            target: "aether_substrate::boot",
            workers = WORKERS,
            tick_hz = tick_hz,
            "componentless boot — load a component via aether.control.load_component",
        );

        // Hub connect AFTER every chassis sink is registered (issue #262).
        // Post-ADR-0070 phase 4: hub client lives in `aether-hub`.
        let hub = crate::hub::connect_hub_client(&boot, hub_url.as_deref())?;

        let registry = Arc::clone(&boot.registry);
        let mailer = Arc::clone(&boot.queue);
        let namespace_roots = boot.namespace_roots.clone();
        // ADR-0074 §Decision 5: production chassis configures the
        // cross-class `wait_reply` aborter to broadcast
        // `SubstrateDying` before exit.
        let aborter: Arc<dyn aether_substrate::lifecycle::FatalAborter> = Arc::new(
            aether_substrate::lifecycle::OutboundFatalAborter::new(Arc::clone(&boot.outbound)),
        );

        let driver = HeadlessTimerCapability {
            boot,
            kind_tick,
            kind_frame_stats,
            tick_period,
            hub,
        };

        // ADR-0071 phase B: io / net / log compose through the
        // chassis_builder `.with()` chain. Boot order is declaration
        // order — log first so other capabilities' boot tracing routes
        // through the log capture.
        let io_cap = IoCapability::new(namespace_roots, Arc::clone(&mailer))
            .map_err(|e| BootError::Other(Box::new(e)))?;
        Builder::<HeadlessChassis>::new(registry, Arc::clone(&mailer))
            .with_aborter(aborter)
            .with(LogCapability::new())
            .with(io_cap)
            .with(NetCapability::new(net, Arc::clone(&mailer)))
            .driver(driver)
            .build()
    }
}
