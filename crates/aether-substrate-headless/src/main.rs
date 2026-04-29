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

use aether_kinds::{FrameStats, InputStream, Tick};
use aether_mail::{Kind, encode, encode_empty};
use aether_substrate_core::{
    Chassis, ChassisCapabilities, InputSubscribers, Mailer, Scheduler, SubstrateBoot,
    mail::{Mail, MailboxId},
    subscribers_for,
};

const WORKERS: usize = 2;
const DEFAULT_TICK_HZ: u32 = 60;
const LOG_EVERY_FRAMES: u64 = 120;

/// ADR-0063 fail-fast budget for the per-tick drain barrier. A
/// dispatcher that doesn't quiesce within this window is treated as
/// wedged and the substrate exits cleanly via `fatal_abort`. Same
/// 5-second value the desktop chassis uses — both run the same
/// dispatcher kernel.
const DRAIN_BUDGET: Duration = Duration::from_secs(5);

/// Headless chassis. Owns the tick loop + the bookkeeping every
/// subsequent frame needs. `run(self)` takes ownership and drives
/// the loop forever — the process exits on SIGTERM (hub-spawned
/// substrates) or SIGINT (manual `cargo run`); there's no clean
/// return path because there's no event source that can close.
struct HeadlessChassis {
    queue: Arc<Mailer>,
    input_subscribers: InputSubscribers,
    broadcast_mbox: MailboxId,
    kind_tick: u64,
    kind_frame_stats: u64,
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
            let subs = subscribers_for(&self.input_subscribers, InputStream::Tick);
            for mbox in subs {
                self.queue
                    .push(Mail::new(mbox, self.kind_tick, encode_empty::<Tick>(), 1));
            }
            // ADR-0063: budget-aware drain. Dispatcher deaths or
            // wedges abort the substrate cleanly via `fatal_abort`.
            let summary = self.queue.drain_all_with_budget(DRAIN_BUDGET);
            if let Some((mailbox, waited)) = summary.wedged {
                aether_substrate_core::lifecycle::fatal_abort(
                    &self.outbound,
                    format!("dispatcher wedged: mailbox={mailbox:?} waited={waited:?}"),
                );
            }
            if let Some(first) = summary.deaths.first() {
                for d in &summary.deaths {
                    tracing::error!(
                        target: "aether_substrate::lifecycle",
                        mailbox = ?d.mailbox,
                        mailbox_name = %d.mailbox_name,
                        last_kind = %d.last_kind,
                        reason = %d.reason,
                        "component died; substrate aborting (ADR-0063)",
                    );
                }
                aether_substrate_core::lifecycle::fatal_abort(
                    &self.outbound,
                    format!(
                        "component died: {} (kind {}) — {}",
                        first.mailbox_name, first.last_kind, first.reason,
                    ),
                );
            }

            if frame.is_multiple_of(LOG_EVERY_FRAMES) {
                let stats = FrameStats {
                    frame,
                    triangles: 0,
                };
                self.queue.push(Mail::new(
                    self.broadcast_mbox,
                    self.kind_frame_stats,
                    encode(&stats),
                    1,
                ));
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
    let boot = SubstrateBoot::builder("headless", env!("CARGO_PKG_VERSION"))
        .workers(WORKERS)
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
            |_kind_id: u64,
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
            |_kind_id: u64,
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
            move |kind_id: u64,
                  _kind_name: &str,
                  _origin: Option<&str>,
                  sender,
                  _bytes: &[u8],
                  _count: u32| {
                if kind_id == kind_set_master_gain {
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

    // `aether.io.*` per ADR-0041. Headless gets the same sink as
    // desktop — the io path is purely I/O, no GPU or window surface,
    // so there's nothing chassis-specific to diverge on. Boot-time
    // filesystem failure logs loud and skips the sink (same policy
    // as desktop) rather than failing the whole chassis.
    match aether_substrate_core::io::build_default_registry() {
        Ok((registry, roots)) => {
            tracing::info!(
                target: "aether_substrate::io",
                save = %roots.save.display(),
                assets = %roots.assets.display(),
                config = %roots.config.display(),
                "io adapters registered",
            );
            boot.registry.register_sink(
                "aether.sink.io",
                aether_substrate_core::io::io_sink_handler(registry, Arc::clone(&boot.queue)),
            );
        }
        Err(e) => {
            tracing::error!(
                target: "aether_substrate::io",
                error = %e,
                "io adapter init failed — `io` sink not registered",
            );
        }
    }

    // `aether.net.fetch`: ADR-0043 substrate HTTP egress. Headless
    // runs the asset pipeline, so net is first-class here — same
    // shape as desktop. Deny-by-default via `AETHER_NET_ALLOWLIST`;
    // `AETHER_NET_DISABLE=1` swaps to a nop adapter that replies
    // `Disabled`.
    let net_adapter = aether_substrate_core::net::build_default_adapter();
    boot.registry.register_sink(
        "aether.sink.net",
        aether_substrate_core::net::net_sink_handler(net_adapter, Arc::clone(&boot.queue)),
    );

    // `aether.sink.log`: ADR-0060. Same handler as desktop — guest
    // log mail is independent of GPU / windowing, so headless wires
    // it identically.
    aether_substrate_core::log_sink::register_log_sink(&boot.registry);

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
    // `boot` below — connect_hub_from_env borrows `&boot`.
    let hub = boot.connect_hub_from_env()?;

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
