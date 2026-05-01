// Headless chassis: std-timer tick driver, no window, no GPU. Boots
// componentless (ADR-0010) and runs the same scheduler + control
// plane as desktop; components are loaded at runtime via
// `aether.control.load_component` over the hub. Desktop-only control
// kinds (capture_frame, set_window_mode, platform_info) are handled
// by `chassis::chassis_control_handler`, which replies with an
// explicit `Err { error: "unsupported on headless" }` so callers
// don't stall waiting for a reply that's never coming.

mod chassis;

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use aether_data::{Kind, KindId, encode_empty};
use aether_kinds::{FrameStats, Tick};
use aether_substrate_core::{
    Chassis, ChassisCapabilities, InputSubscribers, Mailer, Scheduler, SubstrateBoot, frame_loop,
    mail::{Mail, MailboxId},
    subscribers_for,
};

/// Wire-stable `EngineInfo.workers` value (ADR-0038: post actor-per-
/// component, the scheduler doesn't read this — it's retained on the
/// hub-protocol wire for compatibility). The shared frame-loop
/// policy (drain budget, frame-stats cadence) lives in
/// `aether_substrate_core::frame_loop`.
const WORKERS: usize = 2;
const DEFAULT_TICK_HZ: u32 = 60;

/// Headless chassis. Owns the tick loop + the bookkeeping every
/// subsequent frame needs. `run(self)` takes ownership and drives
/// the loop forever — the process exits on SIGTERM (hub-spawned
/// substrates) or SIGINT (manual `cargo run`); there's no clean
/// return path because there's no event source that can close.
struct HeadlessChassis {
    queue: Arc<Mailer>,
    input_subscribers: InputSubscribers,
    broadcast_mbox: MailboxId,
    kind_tick: KindId,
    kind_frame_stats: KindId,
    tick_period: Duration,
    /// ADR-0063: passed to `lifecycle::fatal_abort` for the final
    /// `SubstrateDying` broadcast before exit.
    outbound: Arc<aether_substrate_core::HubOutbound>,
    // Held so the scheduler's worker threads + the hub's reader /
    // heartbeat threads stay alive for the life of the chassis.
    _scheduler: Scheduler,
    _hub: Option<aether_substrate_core::HubClient>,
}

impl Chassis for HeadlessChassis {
    const KIND: &'static str = "headless";
    const CAPABILITIES: ChassisCapabilities = ChassisCapabilities {
        has_gpu: false,
        has_window: false,
        has_tcp_listener: false,
    };

    fn run(self) -> wasmtime::Result<()> {
        let started = Instant::now();
        let mut frame: u64 = 0;
        let mut next_deadline = Instant::now() + self.tick_period;
        loop {
            let now = Instant::now();
            if now < next_deadline {
                thread::sleep(next_deadline - now);
            }
            // Catch the deadline up from the current instant rather
            // than the prior target — if a frame overruns (component
            // deliver stalled, hub socket flushed slowly) we resume
            // from now + period instead of trying to burn through
            // backlog, which would just compound the stall.
            next_deadline = Instant::now() + self.tick_period;

            frame += 1;
            let subs = subscribers_for(&self.input_subscribers, Tick::ID);
            for mbox in subs {
                self.queue
                    .push(Mail::new(mbox, self.kind_tick, encode_empty::<Tick>(), 1));
            }
            // ADR-0063 (issue 427: shared `frame_loop::DRAIN_BUDGET`).
            // Budget-aware drain. Dispatcher deaths or wedges abort
            // the substrate cleanly via `fatal_abort`.
            frame_loop::drain_or_abort(&self.queue, &self.outbound);

            if frame.is_multiple_of(frame_loop::LOG_EVERY_FRAMES) {
                frame_loop::emit_frame_stats(
                    &self.queue,
                    self.broadcast_mbox,
                    self.broadcast_mbox,
                    self.kind_frame_stats,
                    frame,
                    0,
                );
                let elapsed = started.elapsed().as_secs_f64().max(0.001);
                tracing::info!(
                    target: "aether_substrate::frame_loop",
                    frame = frame,
                    fps = frame as f64 / elapsed,
                    "headless tick",
                );
            }
        }
    }
}

fn parse_tick_hz_env() -> u32 {
    match std::env::var("AETHER_TICK_HZ") {
        Ok(s) => s
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|&hz| hz > 0)
            .unwrap_or_else(|| {
                tracing::warn!(
                    target: "aether_substrate::boot",
                    value = %s,
                    "AETHER_TICK_HZ unparseable or zero — falling back to default",
                );
                DEFAULT_TICK_HZ
            }),
        Err(_) => DEFAULT_TICK_HZ,
    }
}

fn main() -> wasmtime::Result<()> {
    // Per issue 464, this `main()` is the env-reading edge. Read every
    // chassis-relevant env var into a config struct and thread it
    // through the substrate-core APIs explicitly. Substrate-core
    // itself never reads env from now on.
    let hub_url = std::env::var("AETHER_HUB_URL").ok();
    let net_config = aether_substrate_core::net::NetConfig::from_env();
    let namespace_roots = aether_substrate_core::io::NamespaceRoots::from_env();

    let mut boot = SubstrateBoot::builder("headless", env!("CARGO_PKG_VERSION"))
        .workers(WORKERS)
        .namespace_roots(namespace_roots)
        .chassis_handler(|ctx| Some(chassis::chassis_control_handler(Arc::clone(ctx.outbound))))
        .build()?;

    let kind_tick = boot.registry.kind_id(Tick::NAME).expect("Tick registered");
    let kind_frame_stats = boot
        .registry
        .kind_id(FrameStats::NAME)
        .expect("FrameStats registered");

    // Silent drop for `aether.sink.render` mail. A desktop-designed
    // component loaded on a headless substrate will emit `DrawTriangle`
    // every tick; without this sink, core's mailbox-resolution warn
    // fires at the tick rate and buries every other engine_logs entry.
    boot.registry.register_sink(
        "aether.sink.render",
        Arc::new(
            |_kind: KindId,
             _kind_name: &str,
             _origin: Option<&str>,
             _sender,
             _bytes: &[u8],
             _count: u32| {},
        ),
    );
    // Same deal for `aether.sink.camera` — a desktop-designed camera
    // component will emit camera updates every tick. Headless has no
    // GPU to upload to, so silently discard.
    boot.registry.register_sink(
        "aether.sink.camera",
        Arc::new(
            |_kind: KindId,
             _kind_name: &str,
             _origin: Option<&str>,
             _sender,
             _bytes: &[u8],
             _count: u32| {},
        ),
    );
    // `aether.audio.*` per ADR-0039 Phase 2. Headless has no audio
    // device, so NoteOn / NoteOff are discarded silently (keeping the
    // mailbox resolvable so desktop-designed music components loaded
    // on headless don't warn-storm). SetMasterGain replies Err so
    // agents attempting to control audio on a chassis that can't
    // produce it fail fast rather than hang.
    let kind_set_master_gain = boot
        .registry
        .kind_id(aether_kinds::SetMasterGain::NAME)
        .expect("SetMasterGain registered");
    let outbound_for_audio_sink = Arc::clone(&boot.outbound);
    boot.registry.register_sink(
        "aether.sink.audio",
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
                        &aether_kinds::SetMasterGainResult::Err {
                            error: "unsupported on headless chassis — no audio device".to_owned(),
                        },
                    );
                }
                // NoteOn / NoteOff fall through silently.
            },
        ),
    );

    // `aether.io.*` per ADR-0041. Same capability as desktop —
    // the io path is purely I/O, no GPU or window surface, so
    // there's nothing chassis-specific to diverge on. ADR-0070
    // phase 3 wraps it as a native capability with fail-fast boot
    // semantics (ADR-0063); pre-phase-3 behavior was log-and-skip
    // on adapter init failure.
    boot.add_capability(aether_substrate_core::capabilities::IoCapability::new(
        boot.namespace_roots.clone(),
    ))?;

    // `aether.net.fetch`: ADR-0043 substrate HTTP egress. Headless
    // runs the asset pipeline, so net is first-class here — same
    // shape as desktop. Deny-by-default via `AETHER_NET_ALLOWLIST`;
    // `AETHER_NET_DISABLE=1` swaps to a nop adapter that replies
    // `Disabled`. The `NetConfig` was built from env at the top of
    // `main` (issue 464). ADR-0070 phase 3 wraps it as a native
    // capability with its own dispatcher thread.
    boot.add_capability(aether_substrate_core::capabilities::NetCapability::new(
        net_config,
    ))?;

    // `aether.sink.log`: ADR-0060. Same capability as desktop — guest
    // log mail is independent of GPU / windowing, so headless wires
    // it identically.
    boot.add_capability(aether_substrate_core::capabilities::LogCapability::new())?;

    let tick_hz = parse_tick_hz_env();
    let tick_period = Duration::from_nanos(1_000_000_000 / u64::from(tick_hz));
    tracing::info!(
        target: "aether_substrate::boot",
        workers = WORKERS,
        tick_hz = tick_hz,
        "componentless boot — load a component via aether.control.load_component",
    );

    // Connect to the hub LAST, after every chassis sink is registered.
    // Before this returns no hub-driven `load_component` can race
    // ahead of the chassis setup and bind a chassis sink name to a
    // component (issue #262). Must happen before moving fields out of
    // `boot` below — connect_hub borrows `&boot`. Per issue 464,
    // `hub_url` was read from env at the top of `main`.
    let hub = boot.connect_hub(hub_url.as_deref())?;

    let chassis = HeadlessChassis {
        queue: boot.queue,
        input_subscribers: boot.input_subscribers,
        broadcast_mbox: boot.broadcast_mbox,
        kind_tick,
        kind_frame_stats,
        tick_period,
        outbound: boot.outbound,
        _scheduler: boot.scheduler,
        _hub: hub,
    };
    tracing::info!(
        target: "aether_substrate::boot",
        kind = HeadlessChassis::KIND,
        has_gpu = HeadlessChassis::CAPABILITIES.has_gpu,
        has_window = HeadlessChassis::CAPABILITIES.has_window,
        has_tcp_listener = HeadlessChassis::CAPABILITIES.has_tcp_listener,
        "chassis initialised",
    );
    chassis.run()
}
