// Headless chassis: std-timer tick driver, no window, no GPU. Boots
// componentless (ADR-0010) and runs the same scheduler + control
// plane as desktop; components are loaded at runtime via
// `aether.control.load_component` over the hub. Desktop-only control
// kinds (capture_frame, set_window_mode, platform_info) are handled
// by `chassis::chassis_control_handler`, which replies with an
// explicit `Err { error: "unsupported on headless" }` so callers
// don't stall waiting for a reply that's never coming.

mod chassis;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::thread;
use std::time::{Duration, Instant};

use aether_hub_protocol::{ClaudeAddress, EngineMailFrame, EngineToHub};
use aether_kinds::{FrameStats, InputStream, Tick};
use aether_mail::{Kind, encode, encode_empty};
use aether_substrate_core::{
    AETHER_CONTROL, Chassis, ChassisCapabilities, ControlPlane, HUB_CLAUDE_BROADCAST, HubClient,
    HubOutbound, InputSubscribers, MailQueue, Registry, Scheduler, SubstrateCtx, host_fns,
    log_capture,
    mail::{Mail, MailboxId},
    new_subscribers, subscribers_for,
};
use wasmtime::{Engine, Linker};

const WORKERS: usize = 2;
const DEFAULT_TICK_HZ: u32 = 60;
const LOG_EVERY_FRAMES: u64 = 120;

/// Headless chassis. Owns the tick loop + the bookkeeping every
/// subsequent frame needs. `run(self)` takes ownership and drives
/// the loop forever — the process exits on SIGTERM (hub-spawned
/// substrates) or SIGINT (manual `cargo run`); there's no clean
/// return path because there's no event source that can close.
struct HeadlessChassis {
    queue: Arc<MailQueue>,
    input_subscribers: InputSubscribers,
    broadcast_mbox: MailboxId,
    kind_tick: u64,
    kind_frame_stats: u64,
    tick_period: Duration,
    _scheduler: Scheduler,
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
            self.queue.wait_idle();

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
    let outbound = HubOutbound::disconnected();
    log_capture::init(Arc::clone(&outbound));

    let engine = Arc::new(Engine::default());
    let registry = Arc::new(Registry::new());

    let boot_descriptors = aether_kinds::descriptors::all();
    for d in &boot_descriptors {
        registry
            .register_kind_with_descriptor(d.clone())
            .expect("duplicate kind in substrate init");
    }
    let kind_tick = registry.kind_id(Tick::NAME).expect("Tick registered");
    let kind_frame_stats = registry
        .kind_id(FrameStats::NAME)
        .expect("FrameStats registered");

    // Silent drop for `render` mail. A desktop-designed component
    // loaded on a headless substrate will emit `DrawTriangle` every
    // tick; without this sink, core's mailbox-resolution warn fires
    // at the tick rate and buries every other engine_logs entry.
    // Registering a nop sink tells the substrate "yes, mail to
    // render is expected here even though there's no renderer."
    registry.register_sink(
        "render",
        Arc::new(
            |_kind_id: u64,
             _kind_name: &str,
             _origin: Option<&str>,
             _sender,
             _bytes: &[u8],
             _count: u32| {},
        ),
    );

    let broadcast_mbox = {
        let outbound = Arc::clone(&outbound);
        registry.register_sink(
            HUB_CLAUDE_BROADCAST,
            Arc::new(
                move |_kind_id: u64,
                      kind_name: &str,
                      origin: Option<&str>,
                      _sender,
                      bytes: &[u8],
                      _count: u32| {
                    if kind_name.is_empty() {
                        tracing::warn!(
                            target: "aether_substrate::broadcast",
                            "{HUB_CLAUDE_BROADCAST} received mail with unregistered kind — dropping",
                        );
                        return;
                    }
                    outbound.send(EngineToHub::Mail(EngineMailFrame {
                        address: ClaudeAddress::Broadcast,
                        kind_name: kind_name.to_owned(),
                        payload: bytes.to_vec(),
                        origin: origin.map(str::to_owned),
                    }));
                },
            ),
        )
    };

    let queue = Arc::new(MailQueue::new());

    let mut linker: Linker<SubstrateCtx> = Linker::new(&engine);
    host_fns::register(&mut linker)?;
    let linker = Arc::new(linker);

    let scheduler = Scheduler::new(
        Arc::clone(&registry),
        Arc::clone(&queue),
        HashMap::new(),
        WORKERS,
    );

    let input_subscribers = new_subscribers();

    {
        let control_plane = ControlPlane {
            engine: Arc::clone(&engine),
            linker: Arc::clone(&linker),
            registry: Arc::clone(&registry),
            queue: Arc::clone(&queue),
            outbound: Arc::clone(&outbound),
            components: scheduler.components().clone(),
            input_subscribers: Arc::clone(&input_subscribers),
            default_name_counter: Arc::new(AtomicU64::new(0)),
            chassis_handler: Some(chassis::chassis_control_handler(Arc::clone(&outbound))),
        };
        registry.register_sink(AETHER_CONTROL, control_plane.into_sink_handler());
    }

    let _hub = match std::env::var("AETHER_HUB_URL") {
        Ok(url) => match HubClient::connect(
            url.as_str(),
            "headless",
            env!("CARGO_PKG_VERSION"),
            boot_descriptors.clone(),
            Arc::clone(&registry),
            Arc::clone(&queue),
            Arc::clone(&outbound),
        ) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::error!(
                    target: "aether_substrate::boot",
                    url = %url,
                    error = %e,
                    "hub connect failed",
                );
                None
            }
        },
        Err(_) => None,
    };

    let tick_hz = parse_tick_hz_env();
    let tick_period = Duration::from_nanos(1_000_000_000 / u64::from(tick_hz));
    tracing::info!(
        target: "aether_substrate::boot",
        workers = WORKERS,
        tick_hz = tick_hz,
        "componentless boot — load a component via aether.control.load_component",
    );

    let chassis = HeadlessChassis {
        queue,
        input_subscribers,
        broadcast_mbox,
        kind_tick,
        kind_frame_stats,
        tick_period,
        _scheduler: scheduler,
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
