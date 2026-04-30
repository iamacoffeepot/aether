//! `TestBench` — the in-process driver for the test-bench chassis (ADR-0067).
//!
//! Boots the same substrate machinery `main.rs` does, but instead
//! of dialing a hub it attaches a loopback channel to `outbound`.
//! Substrate-emitted replies arrive on `loopback_rx` so the test
//! thread can correlate them to its requests by `correlation_id`.
//!
//! The chassis-control handler is the same one the binary uses —
//! it pushes `Advance` / `CaptureRequested` events onto the events
//! channel. `TestBench::advance` drains the queue (which lets the
//! handler run), pumps any pending events through `run_frame`
//! synchronously, then drains the loopback for the matching reply.
//!
//! Reply correlation: every API call gets a fresh `correlation_id`.
//! The substrate echoes it on the reply per ADR-0042, so multiple
//! in-flight requests would be unambiguous — though `TestBench`'s
//! synchronous shape means at most one is ever outstanding.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use aether_hub_protocol::{ClaudeAddress, EngineToHub, SessionToken, Uuid};
use aether_kinds::{
    Advance, AdvanceResult, CaptureFrame, CaptureFrameResult, FrameStats, InputStream, Tick,
};
use aether_mail::{Kind, encode, encode_empty, encode_struct, mailbox_id_from_name};
// `encode` is used for FrameStats (cast-shape); `encode_struct` is
// used for control kinds (postcard-shape).
use aether_substrate_core::{
    HubOutbound, InputSubscribers, Mailer, ReplyTarget, ReplyTo, SubstrateBoot,
    capture::CaptureQueue,
    mail::{Mail, MailboxId},
    subscribers_for,
};

use crate::chassis;
use crate::events::{ChassisEvent, EventReceiver, channel as event_channel};
use crate::render::{Gpu, IDENTITY_VIEW_PROJ, VERTEX_BUFFER_BYTES};

const WORKERS: usize = 2;
const LOG_EVERY_FRAMES: u64 = 120;
const DRAW_TRIANGLE_BYTES: usize = 72;
const DRAIN_BUDGET: Duration = Duration::from_secs(5);

/// Default offscreen target dimensions when the caller picks
/// `start()` (no explicit size). 800x600 matches the scenario harness
/// convention — large enough that `min_non_bg_pixels` thresholds
/// discriminate, small enough that capture readback is cheap.
pub const DEFAULT_WIDTH: u32 = 800;
pub const DEFAULT_HEIGHT: u32 = 600;

/// Errors `TestBench` API methods surface. `Boot` covers any failure
/// in the substrate's `build()`; `Decode` covers postcard reply
/// decode failures (rare — implies a kind shape mismatch); `Timeout`
/// covers replies that never arrive (chassis hung or wrong target);
/// `Advance` and `Capture` pass through `Err` variants from the
/// substrate's reply.
#[derive(Debug)]
pub enum TestBenchError {
    Boot(String),
    Decode(String),
    Timeout {
        expected: &'static str,
        pumped_iterations: u32,
    },
    Advance(String),
    Capture(String),
    UnknownMailbox(String),
}

impl fmt::Display for TestBenchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Boot(e) => write!(f, "substrate boot failed: {e}"),
            Self::Decode(e) => write!(f, "decode reply: {e}"),
            Self::Timeout {
                expected,
                pumped_iterations,
            } => write!(
                f,
                "expected {expected} reply, did not arrive within {pumped_iterations} pump iterations",
            ),
            Self::Advance(e) => write!(f, "advance failed: {e}"),
            Self::Capture(e) => write!(f, "capture failed: {e}"),
            Self::UnknownMailbox(name) => write!(f, "unknown mailbox: {name}"),
        }
    }
}

impl std::error::Error for TestBenchError {}

/// In-process test-bench driver. Owns the substrate, runs the
/// chassis events loop synchronously inside its API methods, routes
/// substrate replies through a loopback channel.
///
/// Construction boots a fresh substrate and attaches the loopback;
/// drop tears it down (the held `_boot` is the lifetime guard for
/// the scheduler workers). Methods are `&mut self` because they
/// mutate frame state and pump events; concurrent calls are not
/// supported.
pub struct TestBench {
    queue: Arc<Mailer>,
    registry: Arc<aether_substrate_core::Registry>,
    outbound: Arc<HubOutbound>,
    loopback_rx: mpsc::Receiver<EngineToHub>,

    capture_queue: CaptureQueue,
    events_rx: EventReceiver,

    gpu: Gpu,
    frame_vertices: Arc<Mutex<Vec<u8>>>,
    camera_state: Arc<Mutex<[f32; 16]>>,
    triangles_rendered: Arc<AtomicU64>,

    input_subscribers: InputSubscribers,
    broadcast_mbox: MailboxId,
    kind_tick: u64,
    kind_frame_stats: u64,

    started: Instant,
    frame: u64,
    next_correlation_id: AtomicU64,
    /// Stable session identity for reply addressing. The substrate
    /// echoes this on every reply addressed to `ReplyTarget::Session`,
    /// so the loopback receiver can recognise its own replies.
    session: SessionToken,

    /// Replies that arrived for correlation_ids we haven't waited
    /// for yet. Single-threaded callers won't accumulate entries
    /// here; the field exists so an out-of-order reply (e.g. a
    /// late-arriving frame) doesn't get silently dropped.
    stashed_replies: HashMap<u64, EngineToHub>,

    /// Kind names of mail observed via the chassis-owned sinks
    /// (`aether.sink.render`, `aether.sink.camera`) plus broadcast /
    /// session-zero frames that arrived on the loopback. Used by
    /// scenario assertions like `Check::MailObserved`. Limitation
    /// (v1): mail addressed to other sinks (`aether.sink.io`,
    /// `aether.sink.log`) and direct component-to-component mail does
    /// not show up here — those flows don't pass through outbound and
    /// are not observed by the chassis-owned sinks the bench wraps.
    observed_kinds: Arc<Mutex<Vec<String>>>,

    /// Lifetime guard. Boot owns the scheduler; dropping the
    /// TestBench drops the boot which joins the worker threads.
    _boot: SubstrateBoot,
}

/// Fixed UUID used as the `SessionToken` for in-process replies.
/// Any non-zero literal works — the substrate just echoes whatever
/// it's handed in `ReplyTarget::Session`. Spelled out as a constant
/// so the boot path is reproducible and the value shows up in logs.
const TESTBENCH_SESSION_UUID: u128 = 0x7E57_BE7C_C0FF_EE15_AE7E_7BE7_5E55_1077;

impl TestBench {
    /// Boot a TestBench at the default 800x600 offscreen size.
    pub fn start() -> Result<Self, TestBenchError> {
        Self::start_with_size(DEFAULT_WIDTH, DEFAULT_HEIGHT)
    }

    /// Boot a TestBench with a specific offscreen target size.
    /// Width / height are clamped to a minimum of 1 inside `Gpu::new`.
    pub fn start_with_size(width: u32, height: u32) -> Result<Self, TestBenchError> {
        let capture_queue = CaptureQueue::new();
        let (events_tx, events_rx) = event_channel();

        let boot = SubstrateBoot::builder("test-bench", env!("CARGO_PKG_VERSION"))
            .workers(WORKERS)
            .chassis_handler({
                let cq = capture_queue.clone();
                let tx = events_tx.clone();
                move |ctx| {
                    Some(chassis::chassis_control_handler(
                        cq,
                        tx,
                        Arc::clone(ctx.registry),
                        Arc::clone(ctx.queue),
                        Arc::clone(ctx.outbound),
                    ))
                }
            })
            .build()
            .map_err(|e| TestBenchError::Boot(e.to_string()))?;

        // Attach a loopback to the boot's outbound. Replies the
        // substrate emits via `outbound.send_reply` arrive here.
        let (loopback_tx, loopback_rx) = mpsc::channel::<EngineToHub>();
        boot.outbound.attach(loopback_tx);

        let kind_tick = boot.registry.kind_id(Tick::NAME).expect("Tick registered");
        let kind_frame_stats = boot
            .registry
            .kind_id(FrameStats::NAME)
            .expect("FrameStats registered");

        let observed_kinds = Arc::new(Mutex::new(Vec::<String>::new()));

        let frame_vertices = Arc::new(Mutex::new(Vec::<u8>::with_capacity(VERTEX_BUFFER_BYTES)));
        let triangles_rendered = Arc::new(AtomicU64::new(0));
        register_render_sink(&boot, &frame_vertices, &triangles_rendered, &observed_kinds);

        let camera_state = Arc::new(Mutex::new(IDENTITY_VIEW_PROJ));
        register_camera_sink(&boot, &camera_state, &observed_kinds);

        if let Ok((reg, _roots)) = aether_substrate_core::io::build_default_registry() {
            boot.registry.register_sink(
                "aether.sink.io",
                aether_substrate_core::io::io_sink_handler(reg, Arc::clone(&boot.queue)),
            );
        }
        aether_substrate_core::log_sink::register_log_sink(&boot.registry);

        let gpu = Gpu::new(width, height);

        // Drop the local events_tx so events_rx hangs up cleanly
        // once every chassis_control_handler clone is released.
        drop(events_tx);

        let queue = Arc::clone(&boot.queue);
        let outbound = Arc::clone(&boot.outbound);
        let registry = Arc::clone(&boot.registry);
        let input_subscribers = boot.input_subscribers.clone();
        let broadcast_mbox = boot.broadcast_mbox;

        Ok(Self {
            queue,
            registry,
            outbound,
            loopback_rx,
            capture_queue,
            events_rx,
            gpu,
            frame_vertices,
            camera_state,
            triangles_rendered,
            input_subscribers,
            broadcast_mbox,
            kind_tick,
            kind_frame_stats,
            started: Instant::now(),
            frame: 0,
            next_correlation_id: AtomicU64::new(1),
            session: SessionToken(Uuid::from_u128(TESTBENCH_SESSION_UUID)),
            stashed_replies: HashMap::new(),
            observed_kinds,
            _boot: boot,
        })
    }

    /// Count how many mail observations match `kind_name`. Includes
    /// mail observed at the chassis-owned `aether.sink.render` /
    /// `aether.sink.camera` sinks plus any broadcast / session-zero
    /// frames that arrived on the loopback. Mail to other sinks and
    /// direct component-to-component flows are not observed (v1).
    pub fn count_observed(&self, kind_name: &str) -> usize {
        self.observed_kinds
            .lock()
            .unwrap()
            .iter()
            .filter(|n| n.as_str() == kind_name)
            .count()
    }

    /// Snapshot every kind name currently observed, oldest first.
    /// Cheap clone — used for scenario diagnostics when an assert
    /// trips, so the failure message can list "what we did see."
    pub fn observed_kinds(&self) -> Vec<String> {
        self.observed_kinds.lock().unwrap().clone()
    }

    /// Push a fire-and-forget mail into the queue. Recipient is
    /// resolved by name against the registry; kind ids are pulled
    /// from `K::ID` so the caller doesn't need to look anything up.
    /// No reply awaited.
    pub fn send_mail<K>(&self, recipient_name: &str, mail: &K) -> Result<(), TestBenchError>
    where
        K: Kind + serde::Serialize,
    {
        let mailbox = self
            .registry
            .lookup(recipient_name)
            .ok_or_else(|| TestBenchError::UnknownMailbox(recipient_name.to_owned()))?;
        let payload = encode_struct(mail);
        self.queue.push(Mail::new(mailbox, K::ID, payload, 1));
        Ok(())
    }

    /// Bytes-level send for callers that resolve kind+payload at
    /// runtime (the scenario library's descriptor-driven path). Same
    /// recipient lookup as `send_mail` but takes a pre-encoded
    /// `(kind_id, bytes)` tuple — the typed `send_mail<K>` is the
    /// preferred path when `K` is known statically.
    pub fn send_bytes(
        &self,
        recipient_name: &str,
        kind_id: u64,
        bytes: Vec<u8>,
    ) -> Result<(), TestBenchError> {
        let mailbox = self
            .registry
            .lookup(recipient_name)
            .ok_or_else(|| TestBenchError::UnknownMailbox(recipient_name.to_owned()))?;
        self.queue.push(Mail::new(mailbox, kind_id, bytes, 1));
        Ok(())
    }

    /// Send `mail` to `recipient_name` with this bench's session as
    /// the reply target, then pump until a matching reply arrives and
    /// decode it as `R`. The reply must be postcard-encoded — true
    /// for every standard reply kind (`*Result` variants in
    /// `aether-kinds`). Use this for any sink/component whose reply
    /// pattern is "send → await → decode" — e.g. the `aether.sink.io`
    /// `Read`/`Write`/`Delete`/`List` round trips. `advance` and
    /// `capture` are specialisations of this same shape against the
    /// `aether.control` mailbox.
    pub fn send_and_await_reply<K, R>(
        &mut self,
        recipient_name: &str,
        mail: &K,
    ) -> Result<R, TestBenchError>
    where
        K: Kind + serde::Serialize,
        R: serde::de::DeserializeOwned,
    {
        let mailbox = self
            .registry
            .lookup(recipient_name)
            .ok_or_else(|| TestBenchError::UnknownMailbox(recipient_name.to_owned()))?;
        let cid = self.fresh_correlation_id();
        let reply_to = ReplyTo::with_correlation(ReplyTarget::Session(self.session), cid);
        let payload = encode_struct(mail);
        self.queue
            .push(Mail::new(mailbox, K::ID, payload, 1).with_reply_to(reply_to));
        self.pump_until_reply::<R>(cid, std::any::type_name::<R>())
    }

    /// Run `ticks` complete frames synchronously. Each frame
    /// dispatches `Tick` to subscribers, drains the queue, and
    /// renders. Returns once the substrate has replied with
    /// `AdvanceResult::Ok`.
    pub fn advance(&mut self, ticks: u32) -> Result<u32, TestBenchError> {
        let cid = self.fresh_correlation_id();
        self.push_control(&Advance { ticks }, cid);
        match self.pump_until_reply::<AdvanceResult>(cid, "AdvanceResult")? {
            AdvanceResult::Ok { ticks_completed } => Ok(ticks_completed),
            AdvanceResult::Err { error } => Err(TestBenchError::Advance(error)),
        }
    }

    /// Issue a `capture_frame` request with no pre/after mail bundles.
    /// Drains the queue (so any state-changing mail already in flight
    /// settles), runs one render-with-capture cycle, and returns the
    /// PNG bytes. Capture observes the current state — it does not
    /// dispatch `Tick`. Pair with `advance` if the world needs to
    /// advance before the capture.
    pub fn capture(&mut self) -> Result<Vec<u8>, TestBenchError> {
        self.capture_with_mails(Vec::new(), Vec::new())
    }

    /// Same as `capture` but with the two `CaptureFrame` mail bundles
    /// (ADR-0020 §capture_frame). `pre` is dispatched *before* the
    /// readback — its effects appear in the captured frame; `after`
    /// is dispatched *after* the readback — typically cleanup that
    /// restores state the caller flipped for the capture. Matches
    /// the wire shape of the MCP `capture_frame` tool.
    pub fn capture_with_mails(
        &mut self,
        pre: Vec<aether_kinds::MailEnvelope>,
        after: Vec<aether_kinds::MailEnvelope>,
    ) -> Result<Vec<u8>, TestBenchError> {
        let cid = self.fresh_correlation_id();
        self.push_control(
            &CaptureFrame {
                mails: pre,
                after_mails: after,
            },
            cid,
        );
        match self.pump_until_reply::<CaptureFrameResult>(cid, "CaptureFrameResult")? {
            CaptureFrameResult::Ok { png } => Ok(png),
            CaptureFrameResult::Err { error } => Err(TestBenchError::Capture(error)),
        }
    }

    /// Push a control mail addressed to the substrate's `aether.control`
    /// mailbox with our session as the reply target and `cid` as the
    /// correlation id. The reply will surface on `loopback_rx` with
    /// the same `cid` echoed.
    fn push_control<K>(&self, mail: &K, cid: u64)
    where
        K: Kind + serde::Serialize,
    {
        let mailbox = MailboxId(mailbox_id_from_name(aether_substrate_core::AETHER_CONTROL));
        let reply_to = ReplyTo::with_correlation(ReplyTarget::Session(self.session), cid);
        let payload = encode_struct(mail);
        self.queue
            .push(Mail::new(mailbox, K::ID, payload, 1).with_reply_to(reply_to));
    }

    fn fresh_correlation_id(&self) -> u64 {
        // 0 is the "no correlation" sentinel so skip it.
        let id = self.next_correlation_id.fetch_add(1, Ordering::SeqCst);
        if id == 0 {
            self.next_correlation_id.fetch_add(1, Ordering::SeqCst)
        } else {
            id
        }
    }

    /// Pump the event channel and the loopback receiver until a
    /// reply with `cid` arrives, decoded as `R`. Each iteration
    /// fully drains the queue and processes any pending events,
    /// so even multi-tick advances finish in one or two iterations.
    fn pump_until_reply<R>(&mut self, cid: u64, expected: &'static str) -> Result<R, TestBenchError>
    where
        R: serde::de::DeserializeOwned,
    {
        const MAX_ITERATIONS: u32 = 256;

        // Check the stash first.
        if let Some(frame) = self.stashed_replies.remove(&cid) {
            return Self::decode_reply::<R>(frame, expected);
        }

        let mut events_processed = 0u32;
        for iteration in 0..MAX_ITERATIONS {
            // Settle the queue. The control mail we pushed flows
            // through the dispatcher → control plane → chassis
            // handler, which produces an event on `events_rx` for
            // Advance/CaptureRequested kinds.
            self.queue.drain_all_with_budget(DRAIN_BUDGET);

            // Drain any pending chassis events. Each invocation
            // potentially produces a reply on `outbound`.
            while let Ok(event) = self.events_rx.try_recv() {
                self.dispatch_event(event);
                events_processed += 1;
            }

            // Look for our reply on the loopback.
            while let Ok(frame) = self.loopback_rx.try_recv() {
                if let Some(frame_cid) = correlation_of(&frame) {
                    if frame_cid == cid {
                        return Self::decode_reply::<R>(frame, expected);
                    }
                    // Reply for a different cid (rare; out-of-order).
                    self.stashed_replies.insert(frame_cid, frame);
                    continue;
                }
                // Broadcast or session-zero — frame_stats and the
                // like. Record the kind so scenario assertions can
                // observe substrate-emitted broadcasts.
                if let EngineToHub::Mail(m) = &frame {
                    self.observed_kinds
                        .lock()
                        .unwrap()
                        .push(m.kind_name.clone());
                }
            }

            if iteration > 0 && events_processed == 0 {
                // No events surfaced this iteration AND no events
                // last iteration; the chassis is idle. The reply
                // isn't coming.
                return Err(TestBenchError::Timeout {
                    expected,
                    pumped_iterations: iteration + 1,
                });
            }
            events_processed = 0;
        }
        Err(TestBenchError::Timeout {
            expected,
            pumped_iterations: MAX_ITERATIONS,
        })
    }

    fn decode_reply<R>(frame: EngineToHub, expected: &'static str) -> Result<R, TestBenchError>
    where
        R: serde::de::DeserializeOwned,
    {
        match frame {
            EngineToHub::Mail(m) => postcard::from_bytes::<R>(&m.payload).map_err(|e| {
                TestBenchError::Decode(format!("{expected} decode: {e} (kind={})", m.kind_name))
            }),
            other => Err(TestBenchError::Decode(format!(
                "expected {expected} mail frame, got {other:?}"
            ))),
        }
    }

    /// Run one chassis event. Mirrors what the binary's events loop
    /// does — but inline on the test thread instead of on a worker.
    fn dispatch_event(&mut self, event: ChassisEvent) {
        match event {
            ChassisEvent::Advance { reply_to, ticks } => {
                for _ in 0..ticks {
                    self.frame += 1;
                    self.run_frame(/* dispatch_tick */ true);
                }
                self.outbound.send_reply(
                    reply_to,
                    &AdvanceResult::Ok {
                        ticks_completed: ticks,
                    },
                );
            }
            ChassisEvent::CaptureRequested => {
                self.frame += 1;
                self.run_frame(/* dispatch_tick */ false);
            }
        }
    }

    fn run_frame(&mut self, dispatch_tick: bool) {
        if dispatch_tick {
            let subs = subscribers_for(&self.input_subscribers, InputStream::Tick);
            for mbox in subs {
                self.queue
                    .push(Mail::new(mbox, self.kind_tick, encode_empty::<Tick>(), 1));
            }
        }
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

        let verts = std::mem::replace(
            &mut *self.frame_vertices.lock().unwrap(),
            Vec::with_capacity(VERTEX_BUFFER_BYTES),
        );
        let view_proj = *self.camera_state.lock().unwrap();

        match self.capture_queue.take() {
            Some(req) => {
                let result = match self.gpu.render_and_capture(&verts, &view_proj) {
                    Ok(png) => CaptureFrameResult::Ok { png },
                    Err(error) => CaptureFrameResult::Err { error },
                };
                for mail in req.after_mails {
                    self.queue.push(mail);
                }
                self.outbound.send_reply(req.reply_to, &result);
            }
            None => {
                self.gpu.render(&verts, &view_proj);
            }
        }

        if self.frame.is_multiple_of(LOG_EVERY_FRAMES) {
            let triangles = self.triangles_rendered.load(Ordering::Relaxed);
            let stats = FrameStats {
                frame: self.frame,
                triangles,
            };
            self.queue.push(Mail::new(
                self.broadcast_mbox,
                self.kind_frame_stats,
                encode(&stats),
                1,
            ));
            let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
            tracing::info!(
                target: "aether_substrate::frame_loop",
                frame = self.frame,
                fps = self.frame as f64 / elapsed,
                triangles,
                "test-bench in-process frame",
            );
        }
    }
}

/// Pull the correlation_id out of an EngineToHub frame, if any.
/// Mail frames addressed at a `Session` carry a correlation_id;
/// broadcasts and other variants return None.
fn correlation_of(frame: &EngineToHub) -> Option<u64> {
    match frame {
        EngineToHub::Mail(m) if !matches!(m.address, ClaudeAddress::Broadcast) => {
            Some(m.correlation_id)
        }
        _ => None,
    }
}

fn register_render_sink(
    boot: &SubstrateBoot,
    frame_vertices: &Arc<Mutex<Vec<u8>>>,
    triangles_rendered: &Arc<AtomicU64>,
    observed_kinds: &Arc<Mutex<Vec<String>>>,
) {
    let verts_for_sink = Arc::clone(frame_vertices);
    let tris_for_sink = Arc::clone(triangles_rendered);
    let observed_for_sink = Arc::clone(observed_kinds);
    boot.registry.register_sink(
        "aether.sink.render",
        Arc::new(
            move |_kind_id: u64,
                  kind_name: &str,
                  _origin: Option<&str>,
                  _sender: ReplyTo,
                  bytes: &[u8],
                  _count: u32| {
                observed_for_sink.lock().unwrap().push(kind_name.to_owned());
                let mut verts = verts_for_sink.lock().unwrap();
                let available = VERTEX_BUFFER_BYTES.saturating_sub(verts.len());
                let write_len = bytes.len().min(available);
                let write_len = write_len - (write_len % DRAW_TRIANGLE_BYTES);
                if write_len > 0 {
                    verts.extend_from_slice(&bytes[..write_len]);
                    tris_for_sink
                        .fetch_add((write_len / DRAW_TRIANGLE_BYTES) as u64, Ordering::Relaxed);
                }
                if write_len < bytes.len() {
                    tracing::warn!(
                        target: "aether_substrate::render",
                        accepted_bytes = write_len,
                        dropped_bytes = bytes.len() - write_len,
                        cap = VERTEX_BUFFER_BYTES,
                        "render sink dropped triangles beyond fixed vertex buffer",
                    );
                }
            },
        ),
    );
}

fn register_camera_sink(
    boot: &SubstrateBoot,
    camera_state: &Arc<Mutex<[f32; 16]>>,
    observed_kinds: &Arc<Mutex<Vec<String>>>,
) {
    let cam_for_sink = Arc::clone(camera_state);
    let observed_for_sink = Arc::clone(observed_kinds);
    boot.registry.register_sink(
        "aether.sink.camera",
        Arc::new(
            move |_kind_id: u64,
                  kind_name: &str,
                  _origin: Option<&str>,
                  _sender: ReplyTo,
                  bytes: &[u8],
                  _count: u32| {
                observed_for_sink.lock().unwrap().push(kind_name.to_owned());
                if bytes.len() != 64 {
                    tracing::warn!(
                        target: "aether_substrate::camera",
                        got = bytes.len(),
                        expected = 64,
                        "camera sink: payload length mismatch, dropping",
                    );
                    return;
                }
                match bytemuck::try_pod_read_unaligned::<[f32; 16]>(bytes) {
                    Ok(mat) => *cam_for_sink.lock().unwrap() = mat,
                    Err(e) => tracing::warn!(
                        target: "aether_substrate::camera",
                        error = %e,
                        "camera sink: cast failed, dropping",
                    ),
                }
            },
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Probe wgpu for any usable adapter. Headless Linux CI runners
    /// have no Vulkan/GL drivers installed, so adapter discovery
    /// returns `None` — those runs skip rather than panic. macOS
    /// (Metal) and Windows (DX12) always succeed.
    fn has_wgpu_adapter() -> bool {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .is_ok()
    }

    /// Boot, advance one tick, capture, sanity-check the PNG.
    /// The default scene is empty so the captured frame is the
    /// background-clear color uniformly. The test asserts the PNG
    /// is well-formed; deeper visual assertions land in the scenario
    /// library.
    #[test]
    fn boot_advance_capture_round_trip() {
        if !has_wgpu_adapter() {
            eprintln!("skipping: no wgpu adapter available on this runner");
            return;
        }
        let mut tb = TestBench::start_with_size(64, 48).expect("start");
        let ticks_completed = tb.advance(1).expect("advance");
        assert_eq!(ticks_completed, 1);
        let png = tb.capture().expect("capture");
        assert!(
            png.starts_with(&[0x89, 0x50, 0x4E, 0x47]),
            "captured bytes are not a PNG: first 8 bytes={:?}",
            &png.iter().take(8).cloned().collect::<Vec<u8>>(),
        );
    }
}
