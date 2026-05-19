//! `TestBench` — the in-process driver for the test-bench chassis (ADR-0067).
//!
//! Boots the same substrate machinery `main.rs` does, but attaches a
//! [`RecordingBackend`] to `outbound` instead of relying on an external
//! egress target. Substrate-emitted replies arrive on `loopback_rx`
//! as [`EgressEvent`]s so the test thread can correlate them to its
//! requests by `correlation_id`.
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

// Test-only skip diagnostics emit `eprintln!` so `cargo test` runners
// surface a visible "skipping: ..." line alongside `test ... ok`;
// not routed through `tracing` (issue 891).
#![cfg_attr(test, allow(clippy::print_stderr))]

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use aether_data::{Kind, KindId, SessionToken, Uuid, encode_empty, encode_struct};
#[cfg(test)]
use aether_kinds::Tick;
use aether_kinds::{Advance, AdvanceResult, CaptureFrame, CaptureFrameResult};
// `encode_struct` is used for control kinds (postcard-shape); cast-
// shape kinds (e.g. FrameStats) flow through `frame_loop` helpers.
use aether_actor::Actor;
use aether_capabilities::{RenderCapability, fs::NamespaceRoots};
use aether_substrate::{
    EgressEvent, HubOutbound, Mailer, PassiveChassis, RecordingBackend, ReplyTarget, ReplyTo,
    SubstrateBoot,
    capture::CaptureQueue,
    chassis::frame_loop,
    mail::{Mail, MailboxId},
};

use super::chassis::{TestBenchBuild, TestBenchChassis, TestBenchEnv, WORKERS};
use super::events::{ChassisEvent, EventReceiver, channel as event_channel};
use super::render::Gpu;
use serde::de::DeserializeOwned;
use std::any;
use std::error;
use std::thread;

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
/// substrate's reply. `SettlementTimeout` surfaces when a
/// `send_mail` / `send_bytes` chain didn't drain within
/// `SETTLEMENT_TIMEOUT` — issue 834: the bench waits on each
/// pushed chain's `Settled { root }` so the next observation
/// (`capture()`, the next typed send, an assertion) is causally
/// after the producer's full descendant tree dispatched.
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
    SettlementTimeout {
        recipient: String,
        kind_name: &'static str,
    },
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
            Self::SettlementTimeout {
                recipient,
                kind_name,
            } => write!(
                f,
                "send to {recipient:?} ({kind_name}) did not settle within {} s — chain likely has an in_flight leak; check `engine_logs` for stuck mail",
                SETTLEMENT_TIMEOUT.as_secs(),
            ),
        }
    }
}

/// Per-send settlement timeout. Mirrors the `run_frame` tick wait at
/// `bench.rs::run_frame`; long enough to absorb wasm compile + cap
/// dispatcher wake under nextest CPU contention.
const SETTLEMENT_TIMEOUT: Duration = Duration::from_secs(5);

impl error::Error for TestBenchError {}

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
    registry: Arc<aether_substrate::Registry>,
    outbound: Arc<HubOutbound>,
    loopback_rx: mpsc::Receiver<EgressEvent>,

    capture_queue: CaptureQueue,
    events_rx: EventReceiver,

    gpu: Gpu,

    /// `aether.lifecycle` mailbox id, cached at boot. `advance()`
    /// fires one `LifecycleAdvance` here per requested tick; the
    /// lifecycle driver broadcasts `Tick` to `aether.input` (relayed
    /// via the chassis's `initial_subscribers`) and the rest of the
    /// subscriber set per ADR-0082.
    lifecycle_mailbox: MailboxId,
    /// Kind id of [`LifecycleAdvance`], pre-resolved so the advance
    /// loop body stays alloc-free per tick.
    kind_lifecycle_advance: KindId,
    /// Snapshot of every frame-bound capability's pending counter
    /// (ADR-0074 §Decision 5). Today: render. Cloned out of
    /// `PassiveChassis::frame_bound_pending` at boot; `advance` /
    /// the bin's `run_frame` hand it to
    /// `frame_loop::drain_frame_bound_or_abort` so render's inbox
    /// quiesces before `record_frame`.
    frame_bound_pending: Vec<(MailboxId, Arc<AtomicU64>)>,

    frame: u64,
    next_correlation_id: AtomicU64,
    /// Stable session identity for reply addressing. The substrate
    /// echoes this on every reply addressed to `ReplyTarget::Session`,
    /// so the loopback receiver can recognise its own replies.
    session: SessionToken,

    /// Replies that arrived for `correlation_ids` we haven't waited
    /// for yet. Single-threaded callers won't accumulate entries
    /// here; the field exists so an out-of-order reply (e.g. a
    /// late-arriving frame) doesn't get silently dropped.
    stashed_replies: HashMap<u64, EgressEvent>,

    /// Kind names of mail observed via the chassis-owned render sink
    /// (`aether.render` — both `aether.draw_triangle` and
    /// `aether.camera` flow here post-ADR-0074 §Decision 7) plus
    /// broadcast / session-zero frames that arrived on the loopback.
    /// Used by scenario assertions like `Check::MailObserved`.
    /// Limitation (v1): mail addressed to other sinks
    /// (`aether.fs`, `aether.log`) and direct
    /// component-to-component mail does not show up here — those
    /// flows don't pass through outbound and are not observed by the
    /// chassis-owned sinks the bench wraps.
    observed_kinds: Arc<Mutex<Vec<String>>>,

    /// Lifetime guard. Boot owns the scheduler; dropping the
    /// `TestBench` drops the boot which joins the worker threads.
    _boot: SubstrateBoot,

    /// `PassiveChassis<TestBenchChassis>` holding the booted Log +
    /// Render passives via the `chassis_builder` typed map. Held for
    /// the bench's lifetime so the passives' dispatcher threads
    /// stay alive; drops in reverse declaration order before
    /// `_boot`, so render+log shut down before the scheduler joins.
    passive: PassiveChassis<TestBenchChassis>,
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
    #[must_use]
    pub fn size(mut self, width: u32, height: u32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    /// Override the ADR-0041 namespace roots. Forwarded to
    /// `SubstrateBootBuilder::namespace_roots` at boot, so the
    /// `aether.fs` adapter wired by the bench resolves
    /// `save://` / `assets://` / `config://` against these paths
    /// instead of [`NamespaceRoots::from_env`].
    #[must_use]
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
    #[must_use]
    pub fn builder() -> TestBenchBuilder {
        TestBenchBuilder::default()
    }

    /// Boot a `TestBench` at the default 800x600 offscreen size.
    pub fn start() -> Result<Self, TestBenchError> {
        Self::start_with_size(DEFAULT_WIDTH, DEFAULT_HEIGHT)
    }

    /// Boot a `TestBench` with a specific offscreen target size.
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

        // ADR-0071 phase 6: substrate boot + every cap goes through
        // `TestBenchChassis::build_passive` — the same path the
        // binary uses. Io is part of the chain when
        // `namespace_roots` is supplied and pre-validation passes;
        // the chassis warns and skips Io otherwise. Tests that care
        // about io supply tempdir roots through
        // `start_with_namespace_roots`; otherwise the bench skips Io.
        let env = TestBenchEnv {
            name: "test-bench".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            workers: WORKERS,
            observed_kinds: Some(Arc::clone(&observed_kinds)),
            events_tx,
            capture_queue: capture_queue.clone(),
            namespace_roots,
        };
        let TestBenchBuild {
            passive,
            boot,
            render_handles,
            kind_tick,
        } = TestBenchChassis::build_passive(env)
            .map_err(|e| TestBenchError::Boot(e.to_string()))?;

        // Attach a `RecordingBackend` to the boot's outbound. Replies
        // the substrate emits via `outbound.send_reply` arrive here
        // as `EgressEvent::ToSession`, which `pump_until_reply`
        // correlates by `correlation_id`.
        let (recording, loopback_rx) = RecordingBackend::new();
        boot.outbound.attach_backend(Arc::new(recording));

        let gpu = Gpu::new(width, height, render_handles);

        let queue = Arc::clone(&boot.queue);
        let outbound = Arc::clone(&boot.outbound);
        let registry = Arc::clone(&boot.registry);
        let lifecycle_mailbox = aether_data::mailbox_id_from_name(
            <aether_substrate::LifecycleDriverCapability<()> as Actor>::NAMESPACE,
        );
        let kind_lifecycle_advance = <aether_kinds::LifecycleAdvance as Kind>::ID;
        let _ = kind_tick; // PR 3b retired direct Tick push; kept on the
        // build result for wire-compat with binaries that haven't migrated yet.
        let frame_bound_pending = passive.frame_bound_pending();

        Ok(Self {
            queue,
            registry,
            outbound,
            loopback_rx,
            capture_queue,
            events_rx,
            gpu,
            lifecycle_mailbox,
            kind_lifecycle_advance,
            frame_bound_pending,
            frame: 0,
            next_correlation_id: AtomicU64::new(1),
            session: SessionToken(Uuid::from_u128(TESTBENCH_SESSION_UUID)),
            stashed_replies: HashMap::new(),
            observed_kinds,
            _boot: boot,
            passive,
        })
    }

    /// Count how many mail observations match `kind_name`. Includes
    /// mail observed at the chassis-owned `aether.render` sink
    /// (which receives both `aether.draw_triangle` and
    /// `aether.camera` post-ADR-0074 §Decision 7) plus any broadcast
    /// / session-zero frames that arrived on the loopback. Mail to
    /// other sinks and direct component-to-component flows are not
    /// observed (v1).
    ///
    /// # Panics
    /// Panics if the `observed_kinds` mutex is poisoned — fail-fast
    /// per ADR-0063: a poisoned mutex means a prior holder panicked
    /// under the guard.
    pub fn count_observed(&self, kind_name: &str) -> usize {
        self.observed_kinds
            .lock()
            .expect("observed_kinds mutex is never poisoned (ADR-0063 fail-fast)")
            .iter()
            .filter(|n| n.as_str() == kind_name)
            .count()
    }

    /// Snapshot every kind name currently observed, oldest first.
    /// Cheap clone — used for scenario diagnostics when an assert
    /// trips, so the failure message can list "what we did see."
    ///
    /// # Panics
    /// Panics if the `observed_kinds` mutex is poisoned — fail-fast
    /// per ADR-0063: a poisoned mutex means a prior holder panicked
    /// under the guard.
    pub fn observed_kinds(&self) -> Vec<String> {
        self.observed_kinds
            .lock()
            .expect("observed_kinds mutex is never poisoned (ADR-0063 fail-fast)")
            .clone()
    }

    /// Push a typed mail and block until the dispatched chain
    /// settles (ADR-0080 §6). Recipient is resolved by name against
    /// the registry; kind ids are pulled from `K::ID` so the caller
    /// doesn't need to look anything up.
    ///
    /// Issue 834: this is synchronous-on-settle. The mail is minted
    /// as a chassis-root via [`Mailer::push_chassis_root_mail`] so the trace
    /// pipeline tracks the chain; the bench then subscribes to
    /// `Settled { root }` and waits up to `SETTLEMENT_TIMEOUT` for
    /// the chain (the recipient's handler + every descendant mail
    /// it spawned) to drain. By the time this returns, any
    /// subsequent observation (`capture()`, the next send, an
    /// assertion) is causally after the producer's full chain —
    /// no more nudge_tick-style band-aids needed for render-flush
    /// races.
    pub fn send_mail<K>(&self, recipient_name: &str, mail: &K) -> Result<(), TestBenchError>
    where
        K: Kind + serde::Serialize,
    {
        let mailbox = self
            .registry
            .lookup(recipient_name)
            .ok_or_else(|| TestBenchError::UnknownMailbox(recipient_name.to_owned()))?;
        let payload = encode_struct(mail);
        self.push_and_settle(recipient_name, K::NAME, mailbox, K::ID, payload)
    }

    /// Bytes-level send for callers that resolve kind+payload at
    /// runtime (the scenario library's descriptor-driven path). Same
    /// recipient lookup as `send_mail` but takes a pre-encoded
    /// `(kind, bytes)` tuple — the typed `send_mail<K>` is the
    /// preferred path when `K` is known statically.
    ///
    /// Issue 834: synchronous-on-settle, same semantics as
    /// [`Self::send_mail`].
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
        self.push_and_settle(recipient_name, "<bytes>", mailbox, kind, bytes)
    }

    /// Shared body of [`Self::send_mail`] / [`Self::send_bytes`]:
    /// push as a chassis-root mail (so the trace pipeline tracks
    /// the chain) and block on `Settled { root }`. Returns
    /// `SettlementTimeout` if the chain doesn't drain within
    /// [`SETTLEMENT_TIMEOUT`].
    fn push_and_settle(
        &self,
        recipient_name: &str,
        kind_name: &'static str,
        mailbox: MailboxId,
        kind: KindId,
        payload: Vec<u8>,
    ) -> Result<(), TestBenchError> {
        let cid = self.fresh_correlation_id();
        let registry = self.passive.settlement_registry();
        let root = self
            .queue
            .push_chassis_root_mail(cid, mailbox, kind, payload, 1);
        let rx = registry.subscribe_settlement(root);
        match rx.recv_timeout(SETTLEMENT_TIMEOUT) {
            Ok(()) => Ok(()),
            Err(_) => Err(TestBenchError::SettlementTimeout {
                recipient: recipient_name.to_owned(),
                kind_name,
            }),
        }
    }

    /// Issue 607 Phase 3: spawn an instanced actor onto the bench's
    /// chassis (ADR-0079). Returns a [`aether_substrate::SpawnBuilder`]
    /// the caller chains `after_init` / `finish` against — the same
    /// shape callers reach for from the chassis-builder scope. Used by
    /// integration tests that exercise the spawn lifecycle without
    /// going through a parent-actor handler.
    pub fn spawn_actor<'a, A>(
        &'a self,
        subname: aether_substrate::Subname<'a>,
        config: A::Config,
    ) -> aether_substrate::SpawnBuilder<'a, A>
    where
        A: aether_actor::Instanced
            + aether_substrate::NativeActor
            + aether_substrate::NativeDispatch,
    {
        self.passive.spawn_actor::<A>(subname, config)
    }

    /// Borrow the bench's [`aether_substrate::ActorRegistry`]. Used
    /// alongside `spawn_actor` so tests can inspect the live entry's
    /// `MailboxId` directly.
    pub fn actor_registry(&self) -> &Arc<aether_substrate::ActorRegistry> {
        self.passive.actor_registry()
    }

    /// Send `mail` to `recipient_name` with this bench's session as
    /// the reply target, then pump until a matching reply arrives and
    /// decode it as `R`. The reply must be postcard-encoded — true
    /// for every standard reply kind (`*Result` variants in
    /// `aether-kinds`). Use this for any sink/component whose reply
    /// pattern is "send → await → decode" — e.g. the `aether.fs`
    /// `Read`/`Write`/`Delete`/`List` round trips. `advance` and
    /// `capture` are specialisations of this same shape against the
    /// `aether.component` mailbox.
    pub fn send_and_await_reply<K, R>(
        &mut self,
        recipient_name: &str,
        mail: &K,
    ) -> Result<R, TestBenchError>
    where
        K: Kind + serde::Serialize,
        R: DeserializeOwned,
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
        self.pump_until_reply::<R>(cid, any::type_name::<R>())
    }

    /// Run `ticks` complete frames synchronously. Each frame
    /// dispatches `Tick` to subscribers, drains the queue, and
    /// renders. Returns once the substrate has replied with
    /// `AdvanceResult::Ok`.
    pub fn advance(&mut self, ticks: u32) -> Result<u32, TestBenchError> {
        let cid = self.fresh_correlation_id();
        // Issue 603 Phase 4: advance migrated from `aether.control`
        // (chassis_handler closure) onto `aether.test_bench`
        // (`TestBenchCapability`).
        self.push_to_mailbox(
            aether_data::mailbox_id_from_name("aether.test_bench"),
            &Advance { ticks },
            cid,
        );
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
    ///
    /// Post-iamacoffeepot/aether#847 the render cap caches the
    /// most-recently-submitted geometry across frames: the capture's
    /// `record_frame` sees an empty `frame_vertices` (no producer
    /// emit this microsecond) and replays the cache instead of
    /// drawing into a clear-color buffer. Callers no longer need to
    /// poke the loaded component with a `Tick` before each capture
    /// — what the test sees is the geometry the producer last
    /// rendered, which matches "what the user would see right now"
    /// in the same way wgpu / D3D / Vulkan swapchain front buffers
    /// behave.
    pub fn capture(&mut self) -> Result<Vec<u8>, TestBenchError> {
        self.capture_with_mails(Vec::new(), Vec::new())
    }

    /// Same as `capture` but with the two `CaptureFrame` mail bundles
    /// (ADR-0020 §`capture_frame`). `pre` is dispatched *before* the
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
        // Issue 603 Phase 2: capture_frame moved to the render
        // capability's `aether.render` mailbox. Pre-Phase-2 the mail
        // landed on `aether.control` and routed through the
        // chassis_handler closure.
        self.push_to_mailbox(
            aether_data::mailbox_id_from_name(RenderCapability::NAMESPACE),
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

    /// Push a typed mail addressed to a specific chassis-owned mailbox
    /// with our session as the reply target and `cid` as the correlation
    /// id. Issue 603 retired `aether.control` as the catch-all for
    /// chassis-peripheral kinds; each one now routes to its own cap
    /// (`aether.render.capture_frame`, `aether.test_bench.advance`,
    /// `aether.window.set_mode`, etc.).
    fn push_to_mailbox<K>(&self, mailbox: MailboxId, mail: &K, cid: u64)
    where
        K: Kind + serde::Serialize,
    {
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
    /// time to wake up — `FsCapability` and friends poll their mpsc
    /// receivers on a 100ms `recv_timeout`, so without this sleep a
    /// capability-mediated reply (e.g. `aether.fs.write` →
    /// `WriteResult`) can't beat the bail-out check.
    fn pump_until_reply<R>(&mut self, cid: u64, expected: &'static str) -> Result<R, TestBenchError>
    where
        R: DeserializeOwned,
    {
        const MAX_ITERATIONS: u32 = 8_192;
        // Sleep per quiet iteration. 10 ms × QUIET_BUDGET caps total
        // wait around 60 s. Long enough to absorb wasm compile under
        // parallel test contention on a 2-core CI runner (issue 603
        // made `aether.control` mail dispatch through
        // `ComponentHostCapability`'s thread instead of inline on the
        // caller, so a `LoadComponent` step needs the wait to ride
        // out the dispatcher hop + wasmtime compile under high CPU
        // pressure when N test binaries run in parallel).
        const QUIET_SLEEP: Duration = Duration::from_millis(10);
        // How many consecutive quiet iterations to tolerate before
        // giving up.
        const QUIET_BUDGET: u32 = 6_000;

        // Check the stash first.
        if let Some(frame) = self.stashed_replies.remove(&cid) {
            return Self::decode_reply::<R>(frame, expected);
        }

        let mut quiet_iterations = 0u32;
        for iteration in 0..MAX_ITERATIONS {
            // The control mail we pushed flows through the dispatcher
            // → control plane → chassis handler synchronously on push,
            // which produces an event on `events_rx` for Advance /
            // CaptureRequested kinds before this loop body runs.

            // Drain any pending chassis events. Each invocation
            // potentially produces a reply on `outbound`. A
            // `SettlementTimeout` from `dispatch_event` short-circuits
            // the pump — the substrate is stuck and no AdvanceResult
            // is coming, so propagating is faster and more actionable
            // than burning out the quiet-iteration budget waiting on
            // a reply that will never land.
            let mut found_event = false;
            while let Ok(event) = self.events_rx.try_recv() {
                self.dispatch_event(event)?;
                found_event = true;
            }

            // Look for our reply on the loopback.
            let mut found_reply = false;
            while let Ok(event) = self.loopback_rx.try_recv() {
                found_reply = true;
                if let Some(event_cid) = correlation_of(&event) {
                    if event_cid == cid {
                        return Self::decode_reply::<R>(event, expected);
                    }
                    // Reply for a different cid (rare; out-of-order).
                    self.stashed_replies.insert(event_cid, event);
                }
                // Other untracked emission (kinds_changed,
                // mailboxes_changed, log_batch). Ignored — only
                // session-targeted replies matter for advance().
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
                thread::sleep(QUIET_SLEEP);
            }
        }
        Err(TestBenchError::Timeout {
            expected,
            pumped_iterations: MAX_ITERATIONS,
        })
    }

    fn decode_reply<R>(event: EgressEvent, expected: &'static str) -> Result<R, TestBenchError>
    where
        R: DeserializeOwned,
    {
        match event {
            EgressEvent::ToSession {
                kind_name, payload, ..
            } => postcard::from_bytes::<R>(&payload).map_err(|e| {
                TestBenchError::Decode(format!("{expected} decode: {e} (kind={kind_name})"))
            }),
            other => Err(TestBenchError::Decode(format!(
                "expected {expected} reply event, got {other:?}"
            ))),
        }
    }

    /// Run one chassis event. Mirrors what the binary's events loop
    /// does — but inline on the test thread instead of on a worker.
    ///
    /// Returns `SettlementTimeout` if `run_frame`'s per-tick
    /// settlement misses [`SETTLEMENT_TIMEOUT`]. In the Advance
    /// branch we bail mid-loop without sending `AdvanceResult::Ok` so
    /// the `pump_until_reply` caller surfaces the timeout rather than
    /// waiting on a reply that will never arrive — the substrate is
    /// in a stuck state and the test should fail loudly.
    // `event` is owned because the match destructures it; clippy
    // doesn't track the partial-move via the `Advance { reply_to, .. }`
    // pattern.
    #[allow(clippy::needless_pass_by_value)]
    fn dispatch_event(&mut self, event: ChassisEvent) -> Result<(), TestBenchError> {
        match event {
            ChassisEvent::Advance { reply_to, ticks } => {
                for _ in 0..ticks {
                    self.frame += 1;
                    self.run_frame(/* dispatch_tick */ true)?;
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
                self.run_frame(/* dispatch_tick */ false)?;
            }
        }
        Ok(())
    }

    fn run_frame(&mut self, dispatch_tick: bool) -> Result<(), TestBenchError> {
        if dispatch_tick {
            // ADR-0080 §6 settlement gating: push the Tick mail with a
            // chassis-minted `MailId` so the trace pipeline tracks
            // the chain, subscribe to its settlement before pushing,
            // and wait on the receiver after. The chain rooted at
            // this Tick's `MailId` covers everything the Tick
            // triggers — input fanout, subscriber handlers, the
            // tick_observed broadcasts that those subscribers emit,
            // the broadcast cap's egress to outbound. When the
            // chain settles, all derived work is done.
            //
            // Replaces the pre-PR-4 `wait_instanced_quiesce` poll
            // loop (issue #707): the polling deadline guessed at
            // when broadcasts had landed; settlement knows
            // structurally.
            //
            // Strict propagation: pre-iamacoffeepot/aether#845 this
            // was `let _ = rx.recv_timeout(...)` — a workaround for
            // pre-#840 settlement leaks that swallowed silently when
            // chains never settled. With #840's terminal-arm
            // bracketing in place a real timeout here means a
            // genuine in_flight leak in some downstream cap;
            // surfacing it as `SettlementTimeout` lets the failing
            // test name the actual cause instead of timing out
            // generically on the reply pump.
            let registry = self.passive.settlement_registry();
            // ADR-0082 PR 3b: TestBench pushes `LifecycleAdvance` to the
            // lifecycle driver, which broadcasts Tick to `aether.input`
            // (relayed via the chassis's `initial_subscribers`) plus any
            // other subscribers. Settlement on the broadcast root waits
            // for the whole subtree — same property the prior direct-
            // push path had.
            let advance_root = self.queue.push_chassis_root_mail(
                self.fresh_correlation_id(),
                self.lifecycle_mailbox,
                self.kind_lifecycle_advance,
                encode_empty::<aether_kinds::LifecycleAdvance>(),
                1,
            );
            let rx = registry.subscribe_settlement(advance_root);
            if rx.recv_timeout(SETTLEMENT_TIMEOUT).is_err() {
                return Err(TestBenchError::SettlementTimeout {
                    recipient:
                        <aether_substrate::LifecycleDriverCapability<()> as Actor>::NAMESPACE
                            .to_owned(),
                    kind_name: aether_kinds::LifecycleAdvance::NAME,
                });
            }
        }
        // ADR-0074 §Decision 5: render's inbox must quiesce before
        // submit so any DrawTriangle / aether.camera mail this frame
        // is integrated into the recorded pass. Settlement above
        // covers the causal-chain invariant; this preserves the
        // separate per-frame ordering invariant for frame-bound caps.
        // (The pre-Phase-4 component drain barrier is retired;
        // trampoline traps fail-fast directly via
        // `NativeBinding::fatal_abort`.)
        frame_loop::drain_frame_bound_or_abort(&self.frame_bound_pending, &self.outbound);

        match self.capture_queue.take() {
            Some(req) => {
                // iamacoffeepot/aether#860: wait for each pre-mail's
                // causal chain to settle before rendering. Matches the
                // structural settlement gate the Advance path uses for
                // `Tick` above — without this, the cross-thread chain
                // pre-mails kick off (component handler → emitted
                // DrawTriangle → render cap accumulator) races
                // `render_and_capture` and an empty `frame_vertices`
                // falls back to the cache. Empty `pre_settlements`
                // (no pre-mails, or a chassis without trace pipeline)
                // skips the loop cleanly.
                for rx in req.pre_settlements {
                    if rx.recv_timeout(SETTLEMENT_TIMEOUT).is_err() {
                        return Err(TestBenchError::SettlementTimeout {
                            recipient: "capture pre-mail chain".to_owned(),
                            kind_name: "<pre-mail>",
                        });
                    }
                }
                let result = CaptureFrameResult::from(self.gpu.render_and_capture());
                for mail in req.after_mails {
                    self.queue.push(mail);
                }
                self.outbound.send_reply(req.reply_to, &result);
            }
            None => {
                self.gpu.render();
            }
        }

        // ADR-0074 §Decision 5 ordering invariant: keep the frame-bound
        // drain so render and any future frame-bound cap quiesces before
        // the next frame. Pre-#775 this was paired with a periodic
        // FrameStats settlement gate every `LOG_EVERY_FRAMES`; with the
        // observation path retired the drain stands on its own.
        frame_loop::drain_frame_bound_or_abort(&self.frame_bound_pending, &self.outbound);

        Ok(())
    }
}

/// Pull the `correlation_id` out of an `EgressEvent`, if it represents
/// a session-targeted reply. `Broadcast` and the other event shapes
/// aren't replies and return `None` — `pump_until_reply` records the
/// broadcast kind for `observed_kinds` and otherwise ignores them.
fn correlation_of(event: &EgressEvent) -> Option<u64> {
    match event {
        EgressEvent::ToSession { correlation_id, .. } => Some(*correlation_id),
        _ => None,
    }
}

#[cfg(test)]
// Integration tests stage actors, sender threads, and per-step
// assertions inline so the boot/dispatch sequence reads top-to-bottom;
// extracting helpers would scatter the staging context across files.
// Tests also hold capture `Mutex` guards across the assertion block
// so the snapshot reads atomically against the concurrent push path.
#[allow(clippy::too_many_lines, clippy::significant_drop_tightening)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Instant;

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
            &png.iter().take(8).copied().collect::<Vec<u8>>(),
        );
    }

    /// Issue iamacoffeepot/aether#723: chassis-source ticks are minted
    /// via `push_chassis_root_mail`, and the input cap fanout
    /// propagates `(root, parent_mail)` from the inbound through
    /// `NativeCtx::fanout` so each subscriber-bound copy lands in the
    /// same causal chain. Verified by registering a closure-bound
    /// mailbox, subscribing it to ticks, advancing one tick, and
    /// asserting the captured `MailDispatch` carries non-default root +
    /// parent.
    #[test]
    fn tick_fanout_propagates_chassis_root_lineage() {
        use aether_data::{Kind as DataKind, MailId};
        use aether_kinds::SubscribeInput;
        use aether_substrate::mail::registry::MailDispatch;
        use std::sync::Mutex;

        type CapturedRow = (MailId, MailId, Option<MailId>);

        let mut tb = match TestBench::start_with_size(64, 48) {
            Ok(tb) => tb,
            Err(e) => {
                eprintln!("skipping: TestBench boot failed (likely no wgpu adapter): {e}");
                return;
            }
        };

        // Register a synchronous closure mailbox that captures the
        // lineage of every mail it receives. `register_inline` is the
        // correct variant: the handler does immediate work (push into
        // a captured Vec) rather than enqueueing onto a downstream
        // inbox, so the producer-side `Received`/`Finished` bracket
        // belongs on the call site. `validate_subscriber_mailbox`
        // accepts both `Inbox` and `Inline` so the input cap's
        // subscribe path still admits this mailbox. Pre-#845 this
        // used `register_inbox` and the substrate's `run_frame`
        // silently swallowed the resulting in_flight leak; strict
        // propagation surfaces the variant mismatch as a
        // `SettlementTimeout`, which is the right shape — the
        // handler that owns the bracket gets to advertise its
        // contract via the variant choice.
        let captured: Arc<Mutex<Vec<CapturedRow>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_for_handler = Arc::clone(&captured);
        let subscriber_mbox = tb.registry.register_inline(
            "issue_723_test_subscriber",
            Arc::new(move |dispatch: MailDispatch<'_>| {
                captured_for_handler
                    .lock()
                    .expect("test setup: captured mutex is never poisoned")
                    .push((dispatch.mail_id, dispatch.root, dispatch.parent_mail));
            }),
        );

        // Subscribe the closure mailbox to Tick. Goes through the input
        // cap's on_subscribe handler.
        tb.send_mail(
            "aether.input",
            &SubscribeInput {
                kind: Tick::ID,
                mailbox: subscriber_mbox,
            },
        )
        .expect("subscribe");

        // Advance one tick. This calls `push_chassis_root_mail` for the
        // tick (issue 723), which mints a fresh chassis-root MailId and
        // fires `Sent`. The input cap's `on_tick` reads `ctx.in_flight_*`
        // and threads them through `ctx.fanout` to every subscriber.
        let _ = tb.advance(1).expect("advance");

        let captured = captured
            .lock()
            .expect("test setup: captured mutex is never poisoned");
        assert!(
            !captured.is_empty(),
            "subscriber received no mail — fanout never reached it",
        );
        let (mail_id, root, parent) = captured[0];
        // Issue 723 fix: each fanned-out copy gets its own MailId, but
        // the root is inherited from the chassis-root tick and the
        // parent_mail points at it. Pre-fix both would be MailId::NONE
        // (orphaned: ctx.in_flight was NONE because the tick used
        // bare push, AND the fanout used bare push too).
        assert_ne!(
            root,
            MailId::NONE,
            "fanned-out copy should inherit a non-default root"
        );
        assert!(
            parent.is_some_and(|p| p != MailId::NONE),
            "fanned-out copy should carry a non-default parent_mail (got {parent:?})",
        );
        // The fanned-out copy's own mail_id must be distinct from its
        // parent — it's a child node in the trace tree.
        assert_ne!(
            mail_id,
            parent.expect("test setup: parent was asserted non-None above"),
            "fanned-out mail_id should differ from parent (each fanout copy gets a fresh id)"
        );
    }

    /// Issue 607 Phase 3 verify: spawn an instanced actor through
    /// `TestBench::spawn_actor`, exercise `Subname::Counter` +
    /// `Subname::Named`, assert returned `MailboxId` matches the
    /// deterministic full-name hash, confirm reused subnames fail,
    /// and confirm `after_init` mail lands as the actor's first
    /// dispatch.
    #[test]
    fn spawn_instanced_actor_smoke() {
        use aether_actor::{Actor as ActorTrait, HandlesKind, Instanced};
        use aether_data::{Kind as DataKind, KindId as DataKindId, mailbox_id_from_name};
        use aether_substrate::{
            BootError, NativeActor, NativeCtx, NativeDispatch, NativeInitCtx, SpawnError, Subname,
        };
        use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

        #[repr(C)]
        #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
        struct Bump {
            tag: u32,
        }
        impl DataKind for Bump {
            const NAME: &'static str = "test.spawn.bump";
            const ID: DataKindId = DataKindId(0xB0B1_B2B3_B4B5_B6B7);
            fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
                if bytes.len() != size_of::<Self>() {
                    return None;
                }
                Some(bytemuck::pod_read_unaligned(bytes))
            }
            fn encode_into_bytes(&self) -> Vec<u8> {
                bytemuck::bytes_of(self).to_vec()
            }
        }

        struct Child {
            received: Arc<AtomicU32>,
        }
        impl ActorTrait for Child {
            const NAMESPACE: &'static str = "test.spawn.child";
        }
        impl Instanced for Child {}
        impl HandlesKind<Bump> for Child {}
        impl NativeActor for Child {
            type Config = Arc<AtomicU32>;
            fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self { received: config })
            }
        }
        impl NativeDispatch for Child {
            fn __aether_dispatch_envelope(
                &mut self,
                _ctx: &mut NativeCtx<'_>,
                kind: KindId,
                payload: &[u8],
            ) -> Option<()> {
                if kind.0 == Bump::ID.0 {
                    let _ = Bump::decode_from_bytes(payload)?;
                    self.received.fetch_add(1, AtomicOrdering::SeqCst);
                    return Some(());
                }
                None
            }
        }

        let tb = match TestBench::start_with_size(64, 48) {
            Ok(tb) => tb,
            Err(e) => {
                eprintln!("skipping: TestBench boot failed (likely no wgpu adapter): {e}");
                return;
            }
        };

        let received = Arc::new(AtomicU32::new(0));

        // Subname::Counter — first instance, full name "test.spawn.child:0".
        let id_a = tb
            .spawn_actor::<Child>(Subname::Counter, Arc::clone(&received))
            .after_init(Bump { tag: 1 })
            .after_init(Bump { tag: 2 })
            .finish()
            .expect("first counter spawn");
        assert_eq!(
            id_a,
            MailboxId(mailbox_id_from_name("test.spawn.child:0").0),
            "Counter subname allocates from a per-Spawner counter starting at 0"
        );

        // Subname::Named — second instance, full name "test.spawn.child:alpha".
        let id_b = tb
            .spawn_actor::<Child>(Subname::Named("alpha"), Arc::clone(&received))
            .finish()
            .expect("named spawn");
        assert_eq!(
            id_b,
            MailboxId(mailbox_id_from_name("test.spawn.child:alpha").0),
        );

        // Reused subname → SubnameInUse.
        let err = tb
            .spawn_actor::<Child>(Subname::Named("alpha"), Arc::clone(&received))
            .finish()
            .expect_err("reused subname must fail");
        assert!(
            matches!(err, SpawnError::SubnameInUse { .. }),
            "expected SubnameInUse, got {err:?}"
        );

        // Wait briefly for the two pre-loaded `Bump` mails to land in
        // the first instance's dispatcher.
        let deadline = Instant::now() + Duration::from_millis(500);
        while received.load(AtomicOrdering::SeqCst) < 2 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            received.load(AtomicOrdering::SeqCst),
            2,
            "both pre-loaded after_init mails should dispatch to the first instance"
        );

        // Live registry slots are populated by id.
        assert!(
            tb.actor_registry().is_live(id_a),
            "first instance should be Live in the actor registry"
        );
        assert!(
            tb.actor_registry().is_live(id_b),
            "second instance should be Live in the actor registry"
        );
    }
}
