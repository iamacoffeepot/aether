//! Test-bench chassis-registered control-plane handler (ADR-0067)
//! plus the ADR-0071 phase 6 [`TestBenchChassis`] marker +
//! [`TestBenchEnv`] config + [`TestBenchChassis::build_passive`]
//! entry point.
//!
//! Custom control-plane kinds:
//!
//! - `aether.control.capture_frame` — same two-phase resolve / push
//!   / handoff desktop uses, but the handoff target is the chassis
//!   event channel (not a winit `EventLoopProxy`). The `PendingCapture`
//!   itself rides in `CaptureQueue`; the event channel just signals
//!   the loop to wake up.
//! - `aether.test_bench.advance` — pushes an `Advance` event onto the
//!   chassis event channel. The loop runs N ticks (Tick fanout →
//!   drain → render or render-with-capture) and replies once they
//!   complete.
//! - `set_window_mode` / `set_window_title` / `platform_info` —
//!   reply `Err` with an "unsupported on test-bench chassis" message.
//!   Same fail-fast shape headless uses on these.

use std::sync::{Arc, Mutex};

use aether_data::{Kind, KindId};
use aether_hub::HubClient;
use aether_kinds::{
    Advance, AdvanceResult, CaptureFrame, FrameStats, PlatformInfo, SetWindowMode, SetWindowTitle,
    Tick,
};
use aether_substrate_core::capability::BootError;
use aether_substrate_core::chassis_builder::{
    Builder, BuiltChassis, NeverDriver, NoDriver, PassiveChassis,
};
use aether_substrate_core::{
    Chassis, ChassisControlHandler, HubOutbound, Mailer, Registry, ReplyTo, SubstrateBoot,
    capabilities::{
        LogCapability, RenderCapability, RenderConfig, RenderHandles, io::NamespaceRoots,
    },
    capture::{
        CaptureQueue, begin_capture_request, reply_unsupported_platform_info,
        reply_unsupported_window_mode, reply_unsupported_window_title,
    },
    control::decode_payload,
    render::VERTEX_BUFFER_BYTES,
};

use crate::events::{ChassisEvent, EventSender};

/// Wire-stable `EngineInfo.workers` value (ADR-0038: post actor-per-
/// component, the scheduler doesn't read this — it's retained on the
/// hub-protocol wire for compatibility).
pub const WORKERS: usize = 2;

const UNSUPPORTED_WINDOW: &str = "unsupported on test-bench chassis — no window peripherals (set_window_mode, set_window_title, \
     platform_info are desktop-only)";

pub fn chassis_control_handler(
    capture_queue: CaptureQueue,
    events: EventSender,
    registry: Arc<Registry>,
    queue: Arc<Mailer>,
    outbound: Arc<HubOutbound>,
) -> ChassisControlHandler {
    Arc::new(
        move |kind: KindId, kind_name: &str, sender: ReplyTo, bytes: &[u8]| match kind {
            CaptureFrame::ID => {
                let events = events.clone();
                begin_capture_request(
                    &queue,
                    &capture_queue,
                    &registry,
                    &outbound,
                    sender,
                    bytes,
                    move || {
                        events
                            .send(ChassisEvent::CaptureRequested)
                            .map_err(|_| "test-bench chassis shutting down — capture aborted")
                    },
                );
            }
            Advance::ID => {
                handle_advance(&events, &outbound, sender, bytes);
            }
            SetWindowMode::ID => {
                reply_unsupported_window_mode(&outbound, sender, UNSUPPORTED_WINDOW);
            }
            SetWindowTitle::ID => {
                reply_unsupported_window_title(&outbound, sender, UNSUPPORTED_WINDOW);
            }
            PlatformInfo::ID => {
                reply_unsupported_platform_info(&outbound, sender, UNSUPPORTED_WINDOW);
            }
            _ => {
                tracing::warn!(
                    target: "aether_substrate::chassis",
                    kind = %kind_name,
                    "test-bench chassis has no handler for control kind — dropping",
                );
            }
        },
    )
}

/// Decode `Advance { ticks }`, push the request onto the event
/// channel. The tick loop runs `ticks` cycles and replies. A
/// shut-down loop replies `Err` inline.
fn handle_advance(events: &EventSender, outbound: &HubOutbound, sender: ReplyTo, bytes: &[u8]) {
    let payload: Advance = match decode_payload(bytes) {
        Ok(p) => p,
        Err(error) => {
            outbound.send_reply(sender, &AdvanceResult::Err { error });
            return;
        }
    };

    if events
        .send(ChassisEvent::Advance {
            reply_to: sender,
            ticks: payload.ticks,
        })
        .is_err()
    {
        outbound.send_reply(
            sender,
            &AdvanceResult::Err {
                error: "test-bench chassis shutting down — advance aborted".to_owned(),
            },
        );
    }
}

/// ADR-0071 marker type for the test-bench chassis. Carries no
/// fields — the chassis instance is the [`PassiveChassis<TestBenchChassis>`]
/// returned by [`Self::build_passive`]. Test-bench is the embedder-
/// driven (no-driver) chassis: the binary's `main()` and the
/// in-process [`crate::TestBench`] both build through this and drive
/// their own event loops on top.
pub struct TestBenchChassis;

impl Chassis for TestBenchChassis {
    const PROFILE: &'static str = "test-bench";
    /// Phantom driver — test-bench is passive (the embedder is the
    /// driver). Declaring [`NeverDriver`] satisfies the trait bound;
    /// the value is never instantiated because TestBench's build
    /// path goes through `Builder::<_, NoDriver>::build_passive`.
    type Driver = NeverDriver;
    type Env = TestBenchEnv;

    /// Inert by design — test-bench is a passive chassis. Callers
    /// that try to drive it through the trait method get an error
    /// pointing at [`TestBenchChassis::build_passive`], which is
    /// the actual entry point. The trait method exists so
    /// `Builder<TestBenchChassis, _>` can still parameterise over
    /// `Chassis` per ADR-0071.
    fn build(_env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        Err(BootError::Other(Box::new(std::io::Error::other(
            "TestBenchChassis has no driver; use TestBenchChassis::build_passive(env) instead \
             (the binary main() loops on events_rx; the in-process TestBench dispatches per-call)",
        ))))
    }
}

/// Bag of resolved configs the test-bench chassis takes at build
/// time. Constructed by the embedder — the binary's `main()` reads
/// env vars; the in-process [`crate::TestBench`] takes builder
/// args. `events_tx` is captured into the chassis-control closure;
/// the matching `events_rx` rides on [`TestBenchBuild`] for the
/// embedder to drive.
pub struct TestBenchEnv {
    /// Substrate identity for the hub `Hello` handshake (e.g.
    /// `"test-bench"`). Used by both binary and in-process API.
    pub name: String,
    /// Substrate version for the hub `Hello`. Typically
    /// `env!("CARGO_PKG_VERSION")` from the binary; in-process API
    /// supplies the same.
    pub version: String,
    /// Number of workers for the wire-stable `EngineInfo.workers`
    /// field. Defaults to [`WORKERS`].
    pub workers: usize,
    /// Override for the io adapter's filesystem roots; `None` reads
    /// from env via [`NamespaceRoots::from_env`]. The in-process
    /// API uses `Some(tempdir-based)` for isolation; the binary
    /// passes `None`.
    pub namespace_roots: Option<NamespaceRoots>,
    /// Hub URL for `connect_hub`. Binary reads `AETHER_HUB_URL`;
    /// in-process API typically passes `None`.
    pub hub_url: Option<String>,
    /// Optional observation log: when `Some`, both render and
    /// camera dispatchers push every inbound mail's kind name to it.
    /// In-process API uses this to assert what the sinks have seen;
    /// binary passes `None` for zero overhead.
    pub observed_kinds: Option<Arc<Mutex<Vec<String>>>>,
    /// Sender side of the chassis event channel. Cloned into the
    /// chassis-control handler closure; the matching receiver rides
    /// on [`TestBenchBuild`].
    pub events_tx: EventSender,
    /// Capture-handoff slot the chassis-control handler writes
    /// into; the embedder's frame loop drains it on each
    /// `RedrawRequested`-equivalent step.
    pub capture_queue: CaptureQueue,
}

/// Output of [`TestBenchChassis::build_passive`]. Bundles the
/// `PassiveChassis<TestBenchChassis>` (holding the booted Log +
/// Render passives via chassis_builder typed lookup) with the
/// substrate handles the embedder needs to drive its event loop —
/// queue, outbound, kind ids, render accumulator handles, the hub
/// client.
///
/// `boot` is exposed so the embedder can call
/// `boot.add_capability(...)` for io / etc. with whatever failure
/// semantics it wants (binary uses `?`; in-process API silent-skips
/// io on systems without writable default roots).
///
/// The embedder owns the matching `EventReceiver` for whichever
/// `EventSender` it passed into [`TestBenchEnv`]; the build does
/// not need to thread it through.
pub struct TestBenchBuild {
    pub passive: PassiveChassis<TestBenchChassis>,
    pub boot: SubstrateBoot,
    pub render_handles: RenderHandles,
    pub kind_tick: KindId,
    pub kind_frame_stats: KindId,
    pub hub: Option<HubClient>,
}

impl TestBenchChassis {
    /// Build the test-bench chassis: stand up substrate-core
    /// internals via [`SubstrateBoot::builder`], boot Log + Render
    /// as passives via the chassis_builder [`Builder`], connect the
    /// hub if `env.hub_url` is set, and return a [`TestBenchBuild`]
    /// the embedder takes ownership of. The embedder is responsible
    /// for any further capability adds (io with whatever failure
    /// semantics it wants), GPU creation, loopback attach, and
    /// driving the event loop.
    pub fn build_passive(env: TestBenchEnv) -> wasmtime::Result<TestBenchBuild> {
        let TestBenchEnv {
            name,
            version,
            workers,
            namespace_roots,
            hub_url,
            observed_kinds,
            events_tx,
            capture_queue,
        } = env;

        let mut builder = SubstrateBoot::builder(&name, &version)
            .workers(workers)
            .chassis_handler({
                let cq = capture_queue.clone();
                let tx = events_tx.clone();
                move |ctx| {
                    Some(chassis_control_handler(
                        cq,
                        tx,
                        Arc::clone(ctx.registry),
                        Arc::clone(ctx.queue),
                        Arc::clone(ctx.outbound),
                    ))
                }
            });
        if let Some(roots) = namespace_roots {
            builder = builder.namespace_roots(roots);
        }
        let boot = builder.build()?;

        let kind_tick = boot.registry.kind_id(Tick::NAME).expect("Tick registered");
        let kind_frame_stats = boot
            .registry
            .kind_id(FrameStats::NAME)
            .expect("FrameStats registered");

        let render_cap = RenderCapability::new(RenderConfig {
            vertex_buffer_bytes: VERTEX_BUFFER_BYTES,
            observed_kinds,
        });
        let render_handles = render_cap.handles();

        let passive = Builder::<TestBenchChassis, NoDriver>::new(
            Arc::clone(&boot.registry),
            Arc::clone(&boot.queue),
        )
        .with(LogCapability::new())
        .with(render_cap)
        .build_passive()
        .map_err(|e: BootError| wasmtime::Error::msg(format!("chassis build: {e}")))?;

        let hub = aether_hub::connect_hub_client(&boot, hub_url.as_deref())?;

        // The chassis-control closure already cloned `events_tx`;
        // dropping the local copy lets the receiver hang up cleanly
        // once every chassis_control_handler clone is released. The
        // embedder retains its own `EventSender` clone if it wants
        // to send synthetic events; otherwise the chassis_control
        // closure is the only sender.
        drop(events_tx);

        Ok(TestBenchBuild {
            passive,
            boot,
            render_handles,
            kind_tick,
            kind_frame_stats,
            hub,
        })
    }
}
