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

use aether_data::{Kind, KindId, encode_empty, encode_struct};
use aether_hub::wire::{ClaudeAddress, EngineToHub, SessionToken, Uuid};
use aether_kinds::{Advance, AdvanceResult, CaptureFrame, CaptureFrameResult, Tick};
// `encode_struct` is used for control kinds (postcard-shape); cast-
// shape kinds (e.g. FrameStats) flow through `frame_loop` helpers.
use aether_hub::HubProtocolBackend;
use aether_substrate_core::{
    HubOutbound, InputSubscribers, Mailer, PassiveChassis, ReplyTarget, ReplyTo, SubstrateBoot,
    capabilities::{IoCapability, io::NamespaceRoots},
    capture::CaptureQueue,
    frame_loop,
    mail::{Mail, MailboxId},
    subscribers_for,
};

use crate::chassis::{TestBenchBuild, TestBenchChassis, TestBenchEnv, WORKERS};
use crate::events::{ChassisEvent, EventReceiver, channel as event_channel};
use crate::render::Gpu;

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
    /// `triangles_rendered` is read on the FrameStats emit path; the
    /// other accumulator handles (`frame_vertices` / `camera_state`)
    /// retired post-C2 because `RenderRunning::record_frame` drains
    /// them internally.
    triangles_rendered: Arc<AtomicU64>,

    input_subscribers: InputSubscribers,
    broadcast_mbox: MailboxId,
    kind_tick: KindId,
    kind_frame_stats: KindId,

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

    /// `PassiveChassis<TestBenchChassis>` holding the booted Log +
    /// Render passives via the chassis_builder typed map. Held for
    /// the bench's lifetime so the passives' dispatcher threads
    /// stay alive; drops in reverse declaration order before
    /// `_boot`, so render+log shut down before the scheduler joins.
    _passive: PassiveChassis<TestBenchChassis>,
}

/// Fixed UUID used as the `SessionToken` for in-process replies.
/// Any non-zero literal works — the substrate just echoes whatever
/// it's handed in `ReplyTarget::Session`. Spelled out as a constant
/// so the boot path is reproducible and the value shows up in logs.
const TESTBENCH_SESSION_UUID: u128 = 0x7E57_BE7C_C0FF_EE15_AE7E_7BE7_5E55_1077;

/// Builder for [`TestBench`]. Holds the optional config a test wants
/// to override (offscreen target size, ADR-0041 namespace roots).
/// Tests that want full default behaviour skip the builder and call
/// [`TestBench::start`] / [`TestBench::start_with_size`] directly.
///
/// Per issue 464, the `namespace_roots` override lets a test redirect
/// `save://` / `assets://` / `config://` at a tempdir without touching
/// process env. Pair with `tempfile::TempDir` to scope the redirect to
/// a single test.
pub struct TestBenchBuilder {
    width: u32,
    height: u32,
    namespace_roots: Option<NamespaceRoots>,
}

impl Default for TestBenchBuilder {
    fn default() -> Self {
        Self {
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            namespace_roots: None,
        }
    }
}

impl TestBenchBuilder {
    /// Set the offscreen target size. Width / height are clamped to a
    /// minimum of 1 inside `Gpu::new`.
    pub fn size(mut self, width: u32, height: u32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    /// Override the ADR-0041 namespace roots. Forwarded to
    /// `SubstrateBootBuilder::namespace_roots` at boot, so the
    /// `aether.sink.io` adapter wired by the bench resolves
    /// `save://` / `assets://` / `config://` against these paths
    /// instead of [`NamespaceRoots::from_env`].
    pub fn namespace_roots(mut self, roots: NamespaceRoots) -> Self {
        self.namespace_roots = Some(roots);
        self
    }

    /// Boot the bench. Equivalent to `TestBench::start_with_size` for
    /// the default builder; overrides applied via the builder methods
    /// flow through to `SubstrateBoot::builder` and the chassis-side
    /// IO sink wiring.
    pub fn build(self) -> Result<TestBench, TestBenchError> {
        TestBench::start_inner(self.width, self.height, self.namespace_roots)
    }
}

impl TestBench {
    /// Begin a `TestBench` boot. Default size 800x600, no
    /// `NamespaceRoots` override — chained methods on the returned
    /// builder set those.
    pub fn builder() -> TestBenchBuilder {
        TestBenchBuilder::default()
    }

    /// Boot a TestBench at the default 800x600 offscreen size.
    pub fn start() -> Result<Self, TestBenchError> {
        Self::start_with_size(DEFAULT_WIDTH, DEFAULT_HEIGHT)
    }

    /// Boot a TestBench with a specific offscreen target size.
    /// Width / height are clamped to a minimum of 1 inside `Gpu::new`.
    pub fn start_with_size(width: u32, height: u32) -> Result<Self, TestBenchError> {
        Self::start_inner(width, height, None)
    }

    fn start_inner(
        width: u32,
        height: u32,
        namespace_roots: Option<NamespaceRoots>,
    ) -> Result<Self, TestBenchError> {
        let capture_queue = CaptureQueue::new();
        let (events_tx, events_rx) = event_channel();
        let observed_kinds = Arc::new(Mutex::new(Vec::<String>::new()));

        // ADR-0071 phase 6: substrate boot + Log + Render passives go
        // through `TestBenchChassis::build_passive` — the same path
        // the binary uses. The build hands back the SubstrateBoot for
        // io / further capability adds plus the render handles the
        // bench's frame loop reads each tick.
        let env = TestBenchEnv {
            name: "test-bench".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            workers: WORKERS,
            namespace_roots,
            // In-process bench doesn't dial a hub; replies route to
            // the loopback attached below.
            hub_url: None,
            observed_kinds: Some(Arc::clone(&observed_kinds)),
            events_tx,
            capture_queue: capture_queue.clone(),
        };
        let TestBenchBuild {
            passive,
            mut boot,
            render_handles,
            render_running,
            kind_tick,
            kind_frame_stats,
            hub: _hub,
        } = TestBenchChassis::build_passive(env)
            .map_err(|e| TestBenchError::Boot(e.to_string()))?;

        // Attach a loopback to the boot's outbound. Replies the
        // substrate emits via `outbound.send_reply` arrive here.
        let (loopback_tx, loopback_rx) = mpsc::channel::<EngineToHub>();
        boot.outbound
            .attach_backend(Arc::new(HubProtocolBackend::new(loopback_tx)));

        // Io capability on the legacy `boot.add_capability` path.
        // Silent-skip on adapter init failure preserves pre-Phase-3
        // behavior so tests on systems without writable default roots
        // don't fail at the harness layer; tests that care about io
        // supply tempdir roots via the builder.
        if let Err(e) = boot.add_capability(IoCapability::new(boot.namespace_roots.clone())) {
            tracing::warn!(
                target: "aether_substrate::io",
                error = %e,
                "io capability boot failed in TestBench (expected on systems without writable default roots)",
            );
        }

        let gpu = Gpu::new(width, height, render_running);

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
            triangles_rendered: render_handles.triangles_rendered,
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
            _passive: passive,
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
    /// `(kind, bytes)` tuple — the typed `send_mail<K>` is the
    /// preferred path when `K` is known statically.
    pub fn send_bytes(
        &self,
        recipient_name: &str,
        kind: KindId,
        bytes: Vec<u8>,
    ) -> Result<(), TestBenchError> {
        let mailbox = self
            .registry
            .lookup(recipient_name)
            .ok_or_else(|| TestBenchError::UnknownMailbox(recipient_name.to_owned()))?;
        self.queue.push(Mail::new(mailbox, kind, bytes, 1));
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
        let mailbox = aether_kinds::mailboxes::CONTROL;
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
    /// fully drains the queue and processes any pending events.
    /// Quiet iterations (no events surfaced, no reply on loopback)
    /// sleep briefly to give ADR-0070 capability dispatcher threads
    /// time to wake up — `IoCapability` and friends poll their mpsc
    /// receivers on a 100ms `recv_timeout`, so without this sleep a
    /// capability-mediated reply (e.g. `aether.io.write` →
    /// `WriteResult`) can't beat the bail-out check.
    fn pump_until_reply<R>(&mut self, cid: u64, expected: &'static str) -> Result<R, TestBenchError>
    where
        R: serde::de::DeserializeOwned,
    {
        const MAX_ITERATIONS: u32 = 256;
        // Sleep per quiet iteration. 10 ms × QUIET_BUDGET caps total
        // wait around 1 s, well under any realistic test timeout but
        // long enough that the slowest-polling capability (100 ms
        // recv_timeout) gets ~10 wakeups.
        const QUIET_SLEEP: Duration = Duration::from_millis(10);
        // How many consecutive quiet iterations to tolerate before
        // giving up.
        const QUIET_BUDGET: u32 = 100;

        // Check the stash first.
        if let Some(frame) = self.stashed_replies.remove(&cid) {
            return Self::decode_reply::<R>(frame, expected);
        }

        let mut quiet_iterations = 0u32;
        for iteration in 0..MAX_ITERATIONS {
            // Settle the queue. The control mail we pushed flows
            // through the dispatcher → control plane → chassis
            // handler, which produces an event on `events_rx` for
            // Advance/CaptureRequested kinds.
            self.queue.drain_all_with_budget(frame_loop::DRAIN_BUDGET);

            // Drain any pending chassis events. Each invocation
            // potentially produces a reply on `outbound`.
            let mut found_event = false;
            while let Ok(event) = self.events_rx.try_recv() {
                self.dispatch_event(event);
                found_event = true;
            }

            // Look for our reply on the loopback.
            let mut found_reply = false;
            while let Ok(frame) = self.loopback_rx.try_recv() {
                found_reply = true;
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

            if found_event || found_reply {
                quiet_iterations = 0;
            } else {
                quiet_iterations += 1;
                if quiet_iterations >= QUIET_BUDGET {
                    return Err(TestBenchError::Timeout {
                        expected,
                        pumped_iterations: iteration + 1,
                    });
                }
                // Yield to capability dispatcher threads (ADR-0070).
                std::thread::sleep(QUIET_SLEEP);
            }
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
            let subs = subscribers_for(&self.input_subscribers, Tick::ID);
            for mbox in subs {
                self.queue
                    .push(Mail::new(mbox, self.kind_tick, encode_empty::<Tick>(), 1));
            }
        }
        frame_loop::drain_or_abort(&self.queue, &self.outbound);

        match self.capture_queue.take() {
            Some(req) => {
                let result = match self.gpu.render_and_capture() {
                    Ok(png) => CaptureFrameResult::Ok { png },
                    Err(error) => CaptureFrameResult::Err { error },
                };
                for mail in req.after_mails {
                    self.queue.push(mail);
                }
                self.outbound.send_reply(req.reply_to, &result);
            }
            None => {
                self.gpu.render();
            }
        }

        if self.frame.is_multiple_of(frame_loop::LOG_EVERY_FRAMES) {
            let triangles = self.triangles_rendered.load(Ordering::Relaxed);
            frame_loop::emit_frame_stats(
                &self.queue,
                self.broadcast_mbox,
                self.broadcast_mbox,
                self.kind_frame_stats,
                self.frame,
                triangles,
            );
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Boot, advance one tick, capture, sanity-check the PNG.
    /// The default scene is empty so the captured frame is the
    /// background-clear color uniformly. The test asserts the PNG
    /// is well-formed; deeper visual assertions land in the scenario
    /// library.
    ///
    /// This crate can't depend on `aether-scenario`'s `test_helpers`
    /// (the scenario crate already depends on test-bench, so reaching
    /// for `aether_scenario::test_helpers::has_wgpu_adapter` would
    /// produce a circular path-dep). Instead, the unit test lets
    /// `TestBench::start_with_size` fail naturally on driverless
    /// runners and skips on any boot error — the same skip semantics
    /// the helper provides, just keyed off the boot result rather
    /// than a separate adapter probe.
    #[test]
    fn boot_advance_capture_round_trip() {
        let mut tb = match TestBench::start_with_size(64, 48) {
            Ok(tb) => tb,
            Err(e) => {
                eprintln!("skipping: TestBench boot failed (likely no wgpu adapter): {e}");
                return;
            }
        };
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
