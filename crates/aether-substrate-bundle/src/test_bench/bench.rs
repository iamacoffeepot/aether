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
use std::time::{Duration, Instant};

#[cfg(test)]
use aether_capabilities::trace_walk::TreeWalk;
use aether_data::{Kind, KindId, SessionToken, Uuid, encode_empty};
#[cfg(test)]
use aether_kinds::trace::{DescribeTreeResult, TraceTail, TraceTailResult};
use aether_kinds::{Advance, AdvanceResult, CaptureFrame, CaptureFrameResult};
use aether_kinds::{LogTail, LogTailResult, Tick};
// `push_to_mailbox` encodes any sent kind through the descriptor-aware
// `Kind::encode_into_bytes` (cast or postcard per the kind's shape);
// `encode_empty` builds the zero-byte payload for unit lifecycle kinds.
use aether_actor::Addressable;
use aether_capabilities::{RenderCapability, fs::NamespaceRoots};
use aether_substrate::chassis::settlement::{
    TerminalDisposition, WaitOutcome, await_internal_signal,
};
use aether_substrate::{
    EgressEvent, HubOutbound, Mailer, PassiveChassis, RecordingBackend, RingCapacities, Source,
    SourceAddr, SubstrateBoot,
    capture::CaptureQueue,
    mail::{CapabilityRegistry, CostTable, Mail, MailId, MailboxId},
};

use super::chassis::{TestBenchBuild, TestBenchChassis, TestBenchEnv, WORKERS};
use super::events::{ChassisEvent, EventReceiver, channel as event_channel};
use super::render::Gpu;
use crate::chassis_common::SettlementConfig;
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
/// `send_mail` / `send_bytes` chain didn't settle before the
/// settlement-patience backstop (issue 834: the bench waits on each
/// pushed chain's `Settled { root }` so the next observation —
/// `capture()`, the next typed send, an assertion — is causally
/// after the producer's full descendant tree dispatched). Issue 2062:
/// the backstop is a generous deadlock/livelock cap a healthy chain
/// never reaches, so a `SettlementTimeout` names a genuine wedge and
/// carries a `pending` dump of the stuck roots and their counts.
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
        /// Diagnostic dump of the settlement table's pending roots at the
        /// moment the gate wedged — `root → in_flight=N held_open=M`,
        /// comma-joined (or `<none>`). Names the stuck chain so a genuine
        /// deadlock/livelock is actionable, not a bare timeout (issue 2062).
        pending: String,
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
                pending,
            } => write!(
                f,
                "send to {recipient:?} ({kind_name}) did not settle before the patience backstop — a genuine deadlock/livelock in the chain (a healthy chain never reaches this cap); pending roots: {pending}",
            ),
        }
    }
}

/// Per-round settlement patience: the re-arm interval of the escalating
/// wait, i.e. how often [`await_internal_signal`] logs `gate … slow …
/// extending` while a slow-but-healthy chain is still settling. The log
/// heartbeat, not the gate — a chain that settles is unaffected by its
/// value; only the backstop cap (see [`TestBench::settlement_cap`])
/// declares a wedge. Long enough to absorb wasm compile + cap dispatcher
/// wake under nextest CPU contention.
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
    /// lifecycle driver broadcasts the `Tick` stage directly to its
    /// stage subscriber set per ADR-0082 (issue 1490 retired the
    /// `Tick → aether.input` relay).
    lifecycle_mailbox: MailboxId,
    /// Kind id of [`LifecycleAdvance`], pre-resolved so the advance
    /// loop body stays alloc-free per tick.
    kind_lifecycle_advance: KindId,

    frame: u64,
    next_correlation_id: AtomicU64,

    /// Cumulative settlement-patience backstop the settlement gates
    /// (`push_and_settle`, the capture pre-mail wait, `pump_until_event`'s
    /// no-progress deadline) read instead of a hardcoded 30 s constant
    /// (issue 2062). Resolved at boot from `AETHER_SETTLEMENT_CAP_SECS`
    /// (argv > env > default 5 min) via `SettlementConfig`, or pinned by
    /// the builder. A generous deadlock/livelock cap a healthy chain never
    /// reaches under nextest saturation; [`Duration::MAX`] is the "no cap —
    /// wait forever" sentinel (`AETHER_SETTLEMENT_CAP_SECS=0`).
    settlement_cap: Duration,
    /// Stable session identity for reply addressing. The substrate
    /// echoes this on every reply addressed to `SourceAddr::Session`,
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
    /// Read back via [`Self::count_observed`] / [`Self::observed_kinds`]
    /// for scenario assertions.
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
/// it's handed in `SourceAddr::Session`. Spelled out as a constant
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
    pool_workers: Option<usize>,
    log_ring_capacity: Option<usize>,
    trace_ring_capacity: Option<usize>,
    trace_ring_max_capacity: Option<usize>,
    settlement_cap: Option<Duration>,
}

impl Default for TestBenchBuilder {
    fn default() -> Self {
        Self {
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            namespace_roots: None,
            pool_workers: None,
            log_ring_capacity: None,
            trace_ring_capacity: None,
            trace_ring_max_capacity: None,
            settlement_cap: None,
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

    /// Override the scheduler worker-pool size. `None` (the default)
    /// keeps `PoolConfig::default` (`available_parallelism() - 1`, min
    /// 1); `Some(n)` pins the pool to `n` workers. The mail-latency
    /// harness sweeps this to expose how pool size gates fan-out
    /// parallelism and under-load inbox queueing (iamacoffeepot/aether#1057).
    #[must_use]
    pub fn with_workers(mut self, workers: Option<usize>) -> Self {
        self.pool_workers = workers;
        self
    }

    /// Issue 1990: override the per-actor `ActorLogRing` capacity. `None`
    /// (the default) keeps the `aether-actor` const cap
    /// (`DEFAULT_RING_CAP`); `Some(n)` pins it. Per-bench, no process env
    /// — concurrent benches with different caps don't interfere.
    #[must_use]
    pub fn log_ring_capacity(mut self, capacity: Option<usize>) -> Self {
        self.log_ring_capacity = capacity;
        self
    }

    /// Issue 1990: override the per-actor `ActorTraceRing` capacity (and
    /// the chassis-host trace ring). `None` (the default) keeps the
    /// `aether-actor` const cap (`DEFAULT_TRACE_RING_CAP`); `Some(n)`
    /// pins it — a small value lets an eviction test observe
    /// `truncated_before`. Per-bench, no process env.
    #[must_use]
    pub fn trace_ring_capacity(mut self, capacity: Option<usize>) -> Self {
        self.trace_ring_capacity = capacity;
        self
    }

    /// Override the per-actor `ActorTraceRing` (and chassis-host ring)
    /// growth ceiling — the size a saturating ring grows to before it
    /// resumes drop-oldest. `None` (the default) pins the ceiling to the
    /// floor (`trace_ring_capacity`), giving a fixed ring so eviction
    /// tests stay deterministic; `Some(n)` lets a growth test observe the
    /// ring absorbing a burst past its floor up to `n`. Per-bench, no
    /// process env.
    #[must_use]
    pub fn trace_ring_max_capacity(mut self, capacity: Option<usize>) -> Self {
        self.trace_ring_max_capacity = capacity;
        self
    }

    /// Issue 2062: override the settlement-patience backstop the gates
    /// read. `None` (the default) resolves `AETHER_SETTLEMENT_CAP_SECS`
    /// (argv > env > default 5 min) via `SettlementConfig`; `Some(d)`
    /// pins it — a small value lets a wedge test trip a gate fast without
    /// waiting the real multi-minute backstop, and [`Duration::MAX`] is
    /// the "no cap" sentinel. Per-bench, no process env.
    #[must_use]
    pub fn settlement_cap(mut self, cap: Option<Duration>) -> Self {
        self.settlement_cap = cap;
        self
    }

    /// Boot the bench. Equivalent to `TestBench::start_with_size` for
    /// the default builder; overrides applied via the builder methods
    /// flow through to `SubstrateBoot::builder` and the chassis-side
    /// IO sink wiring.
    pub fn build(self) -> Result<TestBench, TestBenchError> {
        // Lower the per-field `Option` overrides onto the `Copy`
        // `RingCapacities`, defaulting each unset field to the
        // `aether-actor` const cap.
        let default = RingCapacities::default();
        let trace = self.trace_ring_capacity.unwrap_or(default.trace);
        let ring_caps = RingCapacities {
            log: self.log_ring_capacity.unwrap_or(default.log),
            trace,
            // The bench defaults the ceiling to the floor (a fixed,
            // non-growing ring) so eviction tests that pin a small floor
            // observe `truncated_before` deterministically; growth is
            // opt-in via `trace_ring_max_capacity`. (Production chassis
            // default the ceiling to `DEFAULT_TRACE_RING_MAX_CAP` instead,
            // via `ActorRingConfig`.)
            trace_max: self.trace_ring_max_capacity.unwrap_or(trace),
        };
        let settlement_cap = self
            .settlement_cap
            .unwrap_or_else(|| SettlementConfig::from_env().to_cap());
        TestBench::start_inner(
            self.width,
            self.height,
            self.namespace_roots,
            self.pool_workers,
            ring_caps,
            settlement_cap,
        )
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
        Self::start_inner(
            width,
            height,
            None,
            None,
            RingCapacities::default(),
            SettlementConfig::from_env().to_cap(),
        )
    }

    fn start_inner(
        width: u32,
        height: u32,
        namespace_roots: Option<NamespaceRoots>,
        pool_workers: Option<usize>,
        ring_caps: RingCapacities,
        settlement_cap: Duration,
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
            pool_workers,
            ring_caps,
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
        // Chassis route-freezing: the test bench wires its loopback driver to
        // the lifecycle cap's own id (its NAMESPACE) — ctx-less harness setup,
        // no sibling resolver in scope.
        #[allow(clippy::disallowed_methods)]
        let lifecycle_mailbox = aether_data::mailbox_id_from_name(
            <aether_capabilities::LifecycleCapability as Addressable>::NAMESPACE,
        );
        let kind_lifecycle_advance = <aether_kinds::LifecycleAdvance as Kind>::ID;
        let _ = kind_tick; // PR 3b retired direct Tick push; kept on the
        // build result for wire-compat with binaries that haven't migrated yet.

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
            frame: 0,
            next_correlation_id: AtomicU64::new(1),
            settlement_cap,
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

    /// Tail `mailbox_name`'s per-actor log ring (ADR-0081). Mirrors
    /// `FleetBench::log_tail` over the existing `send_bytes_and_await`,
    /// so in-process scenario tests can assert guest-emitted
    /// `tracing::warn!` / `tracing::info!` entries without an RPC session.
    ///
    /// `since: None` reads from the oldest retained entry; `Some(n)` returns
    /// only entries with `sequence > n`. `max: 0` resolves to the
    /// substrate-default cap (currently 100). The framework dispatch loop
    /// answers [`LogTail`] for every native actor and wasm trampoline, so
    /// `mailbox_name` is any live mailbox path (e.g.
    /// `"aether.component/aether.embedded:test_fixture_probe"`).
    ///
    /// # Panics
    /// Panics on a decode failure — implies a kind shape mismatch,
    /// matching the fail-fast disposition of [`Self::count_observed`] /
    /// [`Self::observed_kinds`].
    pub fn log_tail(&mut self, mailbox_name: &str, since: Option<u64>) -> LogTailResult {
        let request = LogTail {
            max: 0,
            min_level: None,
            since,
        };
        let payload = self
            .send_bytes_and_await(mailbox_name, LogTail::ID, request.encode_into_bytes())
            .unwrap_or_else(|e| panic!("log_tail send to {mailbox_name:?} failed: {e}"));
        LogTailResult::decode_from_bytes(&payload).unwrap_or_else(|| {
            panic!("log_tail reply from {mailbox_name:?} did not decode as LogTailResult")
        })
    }

    /// Borrow the substrate's queryable [`CapabilityRegistry`]
    /// (iamacoffeepot/aether#1037). The bench shares the same `Mailer`
    /// every cap registers against, so `accepts(MailboxId, KindId)` /
    /// `has_fallback(MailboxId)` here reflect the post-load /
    /// post-replace / post-drop dispatchability surface. Surfaced for
    /// integration tests that exercise the registry through a real
    /// component-load lifecycle.
    #[must_use]
    pub fn capability_registry(&self) -> &Arc<CapabilityRegistry> {
        self.queue.capability_registry()
    }

    /// Borrow the substrate's per-handler [`CostTable`]
    /// (iamacoffeepot/aether#1128). Shares the same `Mailer` the dispatch
    /// fold writes through, so `tail(MailboxId, …)` here reflects the
    /// cells seeded at component construction (and any folded samples).
    /// Surfaced for integration tests that exercise the cost table
    /// through a real component-load lifecycle.
    #[must_use]
    pub fn cost_table(&self) -> &Arc<CostTable> {
        self.queue.cost_table()
    }

    /// Bytes-level fire-and-settle send: resolve `recipient_name` in
    /// the registry, push `(kind, bytes)` as a chassis-root mail, and
    /// block until the dispatched chain settles (ADR-0080 §6). Backs
    /// the `SendMail` op of [`Self::execute`].
    ///
    /// Issue 834: synchronous-on-settle. The mail is minted as a
    /// chassis-root via [`Mailer::push_chassis_root_mail`] so the trace
    /// pipeline tracks the chain; the bench subscribes to
    /// `Settled { root }` and waits up to `SETTLEMENT_TIMEOUT` for the
    /// chain (the recipient's handler + every descendant mail it
    /// spawned) to drain. By the time this returns, any subsequent
    /// observation is causally after the producer's full chain — no
    /// nudge_tick-style band-aids needed for render-flush races.
    pub(crate) fn send_bytes(
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

    /// Body of [`Self::send_bytes`]: push as a chassis-root mail (so
    /// the trace pipeline tracks the chain) and block on
    /// `Settled { root }`. Returns `SettlementTimeout` if the chain
    /// doesn't drain within [`SETTLEMENT_TIMEOUT`].
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
        match await_internal_signal(
            &rx,
            "test_bench.push_and_settle",
            SETTLEMENT_TIMEOUT,
            self.settlement_cap,
            TerminalDisposition::ReplyErr,
        ) {
            WaitOutcome::Settled => Ok(()),
            WaitOutcome::Wedged(_) => Err(self.settlement_timeout(
                recipient_name.to_owned(),
                kind_name,
                "test_bench.push_and_settle",
            )),
        }
    }

    /// Build a [`TestBenchError::SettlementTimeout`] carrying a dump of the
    /// settlement table's currently-pending roots, and log it (issue 2062).
    /// Shared by the settlement gate sites so a wedge — a genuine
    /// deadlock/livelock, since the cap is a generous backstop a healthy
    /// chain never reaches — names the stuck root(s) and their
    /// `(in_flight, held_open)` counts instead of surfacing a bare timeout.
    fn settlement_timeout(
        &self,
        recipient: String,
        kind_name: &'static str,
        gate: &str,
    ) -> TestBenchError {
        let pending = format_pending_roots(
            &self
                .queue
                .trace_handle()
                .settlement_counter()
                .pending_roots(),
        );
        tracing::error!(
            target: "aether_substrate::test_bench",
            gate,
            recipient = %recipient,
            kind = kind_name,
            pending = %pending,
            "settlement gate wedged: chain did not settle before the patience backstop",
        );
        TestBenchError::SettlementTimeout {
            recipient,
            kind_name,
            pending,
        }
    }

    /// Issue 607 Phase 3: spawn an instanced actor onto the bench's
    /// chassis (ADR-0079). Returns a [`aether_substrate::SpawnBuilder`]
    /// the caller chains `after_init` / `finish` against — the same
    /// shape callers reach for from the chassis-builder scope. Used by
    /// integration tests that exercise the spawn lifecycle without
    /// going through a parent-actor handler, and by the perf sweep
    /// harness ([`crate::perf::harness::run_sweep`], #1077) which the
    /// `perf-trial` bin drives. `pub(crate)` — the public `execute`
    /// driver doesn't model spawning.
    pub(crate) fn spawn_actor<'a, A>(
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
    /// alongside `spawn_actor` so the in-crate spawn test can inspect
    /// the live entry's `MailboxId` directly. Test-only, same
    /// rationale as [`Self::spawn_actor`].
    #[cfg(test)]
    pub(crate) fn actor_registry(&self) -> &Arc<aether_substrate::ActorRegistry> {
        self.passive.actor_registry()
    }

    /// iamacoffeepot/aether#1057: inject a chassis-root mail and return its
    /// `MailId` plus a settlement [`Receiver`] that fires when the whole
    /// causal tree drains. Unlike [`Self::send_bytes`] this does NOT
    /// block — the mail-latency harness injects many roots back-to-back
    /// (to build inbox queueing) and waits on the collected receivers
    /// afterward. Subscription is race-safe: `subscribe_settlement`
    /// pre-fires if the tree settled between the push and the subscribe.
    #[cfg(test)]
    pub(crate) fn inject_root(
        &self,
        recipient: MailboxId,
        kind: KindId,
        payload: Vec<u8>,
    ) -> (MailId, crossbeam_channel::Receiver<()>) {
        let cid = self.fresh_correlation_id();
        let registry = self.passive.settlement_registry();
        let root = self
            .queue
            .push_chassis_root_mail(cid, recipient, kind, payload, 1);
        let rx = registry.subscribe_settlement(root);
        (root, rx)
    }

    /// ADR-0086 Phase 3: read the chassis-host trace ring — where the
    /// `Sent` for off-actor / injected root mail (e.g. [`Self::inject_root`])
    /// lands, since it's produced outside any actor's stamped slots.
    /// Per-actor rings are queried via `aether.trace.tail` mail; this
    /// ring belongs to no actor, so the test reads it directly.
    #[cfg(test)]
    pub(crate) fn chassis_host_trace_tail(&self, request: &TraceTail) -> TraceTailResult {
        self.queue.trace_handle().chassis_host_tail(request)
    }

    /// Like [`Self::send_bytes_and_await`] but addresses the recipient
    /// by [`MailboxId`] directly. The trace-tree guided walk (ADR-0086
    /// Phase 3b) discovers recipients as ids from `Sent` events, never
    /// as names — there's no name to resolve back from a hash.
    #[cfg(test)]
    pub(crate) fn send_bytes_and_await_id(
        &mut self,
        mailbox: MailboxId,
        kind: KindId,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>, TestBenchError> {
        let cid = self.fresh_correlation_id();
        let reply_to = Source::with_correlation(SourceAddr::Session(self.session), cid);
        self.queue
            .push(Mail::new(mailbox, kind, payload, 1).with_reply_to(reply_to));
        self.pump_until_reply_bytes(cid, "<await-reply bytes>")
    }

    /// ADR-0086 Phase 3: reconstruct `root`'s trace tree via the
    /// decentralized guided walk over per-actor rings — the in-process
    /// counterpart to the MCP's over-the-wire walk (there is no central
    /// observer post-3c; the rings are the source of truth). Seeds at
    /// `root.sender`
    /// (`CHASSIS_MAILBOX_ID` for an injected root, an actor otherwise),
    /// then fans out across each `Sent`'s recipient. Every ring —
    /// including the chassis-host ring, reached by the ADR-0086 Phase 3b
    /// wire route at `CHASSIS_MAILBOX_ID` — answers the same
    /// `aether.trace.tail` mail, so this drives the identical path the
    /// MCP does. The `root` filter on every tail isolates the tree from
    /// the trace-query traffic itself.
    #[cfg(test)]
    pub(crate) fn describe_tree_walked(&mut self, root: MailId) -> DescribeTreeResult {
        // The in-process harness reaches the substrate's reverse-lookup
        // registry directly, so it resolves each node's thread name
        // (ADR-0102: the resolver is the caller's; the MCP path passes
        // none).
        use aether_substrate::runtime::thread_name;

        let mut walk = TreeWalk::new(root);
        while let Some(mailbox) = walk.next_mailbox() {
            let request = TraceTail {
                max: 0,
                since: None,
                root: Some(root),
            };
            // A send error (no live actor at this id) or an undecodable
            // reply yields no entries; the walk still completes from the
            // rings that do answer.
            let result = self
                .send_bytes_and_await_id(mailbox, TraceTail::ID, request.encode_into_bytes())
                .ok()
                .and_then(|reply| TraceTailResult::decode_from_bytes(&reply))
                .unwrap_or(TraceTailResult::Ok {
                    entries: Vec::new(),
                    next_since: 0,
                    truncated_before: None,
                });
            if let TraceTailResult::Ok { entries, .. } = result {
                walk.absorb(entries);
            }
        }
        walk.finish_with(|tid| thread_name::resolve(tid.0))
    }

    /// Bytes-level request/reply: push `(kind, payload)` to
    /// `recipient_name` with this bench's session as the reply
    /// target, pump until the matching reply arrives, and return its
    /// raw payload bytes. Backs the `SendAndAwait` op of
    /// [`Self::execute`], where the reply type isn't known statically
    /// and the caller decodes on demand via
    /// [`super::ExecutionResult::reply`]. Used for the
    /// component load/replace/drop round trips and the `aether.fs`
    /// `Read`/`Write`/`Delete`/`List` replies — every standard
    /// `*Result` kind is postcard-encoded.
    pub(crate) fn send_bytes_and_await(
        &mut self,
        recipient_name: &str,
        kind: KindId,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>, TestBenchError> {
        let mailbox = self
            .registry
            .lookup(recipient_name)
            .ok_or_else(|| TestBenchError::UnknownMailbox(recipient_name.to_owned()))?;
        let cid = self.fresh_correlation_id();
        let reply_to = Source::with_correlation(SourceAddr::Session(self.session), cid);
        self.queue
            .push(Mail::new(mailbox, kind, payload, 1).with_reply_to(reply_to));
        self.pump_until_reply_bytes(cid, "<await-reply bytes>")
    }

    /// Run `ticks` complete frames synchronously. Each frame
    /// dispatches `Tick` to subscribers, drains the queue, and
    /// renders. Returns once the substrate has replied with
    /// `AdvanceResult::Ok`.
    pub(crate) fn advance(&mut self, ticks: u32) -> Result<u32, TestBenchError> {
        let cid = self.fresh_correlation_id();
        // Issue 603 Phase 4: advance migrated from `aether.control`
        // (chassis_handler closure) onto `aether.test_bench`
        // (`TestBenchCapability`).
        self.push_to_mailbox(
            // Harness route to the bench's own `TestBenchCapability` mailbox by
            // its well-known name — ctx-less driver-side push, no resolver here.
            #[allow(clippy::disallowed_methods)]
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
    pub(crate) fn capture(&mut self) -> Result<Vec<u8>, TestBenchError> {
        self.capture_with_mails(Vec::new(), Vec::new())
    }

    /// Same as `capture` but with the two `CaptureFrame` mail bundles
    /// (ADR-0020 §`capture_frame`). `pre` is dispatched *before* the
    /// readback — its effects appear in the captured frame; `after`
    /// is dispatched *after* the readback — typically cleanup that
    /// restores state the caller flipped for the capture. Matches
    /// the wire shape of the MCP `capture_frame` tool.
    pub(crate) fn capture_with_mails(
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
            // Harness route to the render cap's own id (its NAMESPACE) —
            // ctx-less driver-side push, no resolver here.
            #[allow(clippy::disallowed_methods)]
            aether_data::mailbox_id_from_name(RenderCapability::NAMESPACE),
            &CaptureFrame {
                mails: pre,
                after_mails: after,
                // The `TestBench::capture` API returns the PNG only; the
                // substrate-side verdict path (iamacoffeepot/aether#1777)
                // and similarity path (iamacoffeepot/aether#1780) are
                // exercised through `BenchOp::send_and_await` scenarios.
                checks: Vec::new(),
                similarity: None,
            },
            cid,
        );
        match self.pump_until_reply::<CaptureFrameResult>(cid, "CaptureFrameResult")? {
            CaptureFrameResult::Ok { png, .. } => Ok(png),
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
        K: Kind,
    {
        let reply_to = Source::with_correlation(SourceAddr::Session(self.session), cid);
        let payload = mail.encode_into_bytes();
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
        R: Kind,
    {
        let event = self.pump_until_event(cid, expected)?;
        Self::decode_reply::<R>(event, expected)
    }

    /// Pump until the reply with `cid` arrives, returning the raw
    /// reply payload bytes instead of decoding. Backs
    /// [`Self::send_bytes_and_await`] and the `SendAndAwait` op of
    /// [`Self::execute`], where the reply type is decoded on demand.
    fn pump_until_reply_bytes(
        &mut self,
        cid: u64,
        expected: &'static str,
    ) -> Result<Vec<u8>, TestBenchError> {
        let event = self.pump_until_event(cid, expected)?;
        Self::reply_payload(event, expected)
    }

    /// Pump the event channel and the loopback receiver until a
    /// session-targeted reply with `cid` arrives, returning the raw
    /// [`EgressEvent`]. Shared loop body of [`Self::pump_until_reply`]
    /// (typed decode) and [`Self::pump_until_reply_bytes`] (raw
    /// bytes).
    fn pump_until_event(
        &mut self,
        cid: u64,
        expected: &'static str,
    ) -> Result<EgressEvent, TestBenchError> {
        // Adaptive backoff between quiet polls. A frame's settlement
        // round-trip (driver → pool → settlement registry → reply)
        // completes in ~1 ms, but a flat coarse sleep makes every tick
        // pay that sleep's full granularity (a flat 10 ms cost ~12 ms
        // per `advance(1)` — the harness's entire wall-clock, and it
        // parked the pool between frames so "warm" was really ~100 Hz
        // paced; iamacoffeepot/aether#1079). So poll fine initially to
        // catch the common case promptly, then back off geometrically
        // to a 10 ms cap for genuine quiet — a wait on a slow cap
        // (`FsCapability` polls its inbox at 100 ms) reaches the cap
        // and sleeps coarsely rather than pinning a core. Each sleep
        // yields the CPU, so capability dispatcher threads still run
        // (ADR-0070).
        const BACKOFF_FLOOR: Duration = Duration::from_micros(50);
        const BACKOFF_CAP: Duration = Duration::from_millis(10);
        // Wall-clock budget for consecutive quiet (no-progress) time
        // before giving up — a deadlock/livelock backstop, not the gate a
        // healthy reply meets, so it reads the runtime-configurable
        // settlement cap (issue 2062) rather than a 1-min constant that
        // false-fired under nextest saturation. A deadline rather than an
        // iteration count so the stall timeout is invariant to poll
        // granularity. The default 5 min rides out a wasmtime compile under
        // parallel-test CPU pressure (issue 603 routed `LoadComponent`
        // through `ComponentHostCapability`'s thread, so a load step waits
        // on a dispatcher hop + compile when N test binaries run in
        // parallel); `Duration::MAX` (the no-cap sentinel) waits forever.
        let stall_deadline = self.settlement_cap;

        // Check the stash first.
        if let Some(frame) = self.stashed_replies.remove(&cid) {
            return Ok(frame);
        }

        let mut backoff = BACKOFF_FLOOR;
        let mut last_progress = Instant::now();
        let mut iterations = 0u32;
        loop {
            iterations = iterations.saturating_add(1);
            // The control mail we pushed flows through the dispatcher
            // → control plane → chassis handler synchronously on push,
            // which produces an event on `events_rx` for Advance /
            // CaptureRequested kinds before this loop body runs.

            // Drain any pending chassis events. Each invocation
            // potentially produces a reply on `outbound`. A
            // `SettlementTimeout` from `dispatch_event` short-circuits
            // the pump — the substrate is stuck and no AdvanceResult
            // is coming, so propagating is faster and more actionable
            // than burning out the stall deadline waiting on a reply
            // that will never land.
            let mut progressed = false;
            while let Ok(event) = self.events_rx.try_recv() {
                self.dispatch_event(event)?;
                progressed = true;
            }

            // Look for our reply on the loopback.
            while let Ok(event) = self.loopback_rx.try_recv() {
                progressed = true;
                if let Some(event_cid) = correlation_of(&event) {
                    if event_cid == cid {
                        return Ok(event);
                    }
                    // Reply for a different cid (rare; out-of-order).
                    self.stashed_replies.insert(event_cid, event);
                }
                // Other untracked emission (kinds_changed,
                // mailboxes_changed, log_batch). Ignored — only
                // session-targeted replies matter for advance().
            }

            if progressed {
                backoff = BACKOFF_FLOOR;
                last_progress = Instant::now();
            } else {
                if last_progress.elapsed() >= stall_deadline {
                    return Err(TestBenchError::Timeout {
                        expected,
                        pumped_iterations: iterations,
                    });
                }
                thread::sleep(backoff);
                backoff = (backoff * 2).min(BACKOFF_CAP);
            }
        }
    }

    fn decode_reply<R>(event: EgressEvent, expected: &'static str) -> Result<R, TestBenchError>
    where
        R: Kind,
    {
        match event {
            EgressEvent::ToSession {
                kind_name, payload, ..
            } => {
                // ADR-0100: decode through the kind's declared codec
                // (cast or postcard), not a hardcoded postcard path.
                R::decode_from_bytes(&payload).ok_or_else(|| {
                    TestBenchError::Decode(format!(
                        "{expected} decode failed via Kind::decode_from_bytes (kind={kind_name})"
                    ))
                })
            }
            other => Err(TestBenchError::Decode(format!(
                "expected {expected} reply event, got {other:?}"
            ))),
        }
    }

    /// Extract the raw payload bytes from a session-targeted reply
    /// event. The bytes-level counterpart to [`Self::decode_reply`] —
    /// the caller decodes later via [`super::ExecutionResult::reply`].
    fn reply_payload(
        event: EgressEvent,
        expected: &'static str,
    ) -> Result<Vec<u8>, TestBenchError> {
        match event {
            EgressEvent::ToSession { payload, .. } => Ok(payload),
            other => Err(TestBenchError::Decode(format!(
                "expected {expected} reply event, got {other:?}"
            ))),
        }
    }

    /// Run one chassis event. Mirrors what the binary's events loop
    /// does — but inline on the test thread instead of on a worker.
    ///
    /// Returns the error `run_frame`'s per-tick advance produces if the
    /// chain never settles: a `Timeout` waiting on the driver's
    /// `LifecycleAdvanceComplete` reply (the broadcast subtree leaked an
    /// `in_flight`, or the driver never replied), or a `SettlementTimeout`
    /// from a capture pre-mail chain. In the Advance branch we bail
    /// mid-loop without sending `AdvanceResult::Ok` so the
    /// `pump_until_reply` caller surfaces the timeout rather than
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
            // ADR-0082 PR 3b: TestBench pushes `LifecycleAdvance` to the
            // lifecycle driver, which broadcasts the `Tick` stage directly
            // to its stage subscribers (issue 1490 retired the
            // `Tick → aether.input` relay; components subscribe `Tick` on
            // `aether.lifecycle`). The chain rooted at this advance's
            // `MailId` covers the whole subtree — the stage fanout,
            // subscriber handlers, the tick_observed broadcasts those
            // subscribers emit, the broadcast cap's egress to outbound.
            //
            // iamacoffeepot/aether#999: gate the per-tick wait on the
            // driver's `LifecycleAdvanceComplete` reply rather than the
            // raw broadcast-root settlement channel. The driver's
            // `on_advance` sets `pending = Some(..)` and clears it only
            // in `on_settled`, which runs on the driver's own actor
            // thread after it dequeues the synthesised `Settled` mail —
            // and only then does it reply `LifecycleAdvanceComplete`.
            // Waiting on the raw settlement channel woke the bench (and
            // let it push the next tick's advance) *before* the driver
            // had cleared `pending`, so under parallel-nextest load the
            // next advance hit `pending.is_some()` and warn-dropped one
            // tick (199 broadcasts, not 200). Correlating on the
            // `LifecycleAdvanceComplete` reply — emitted strictly after
            // `pending` clears — closes that race: by the time the reply
            // lands, the driver is ready for the next advance and the
            // whole broadcast subtree has settled (the reply is gated on
            // settlement). Reuses the same reply-correlated loopback wait
            // (`pump_until_reply`) the `advance()` / `capture()` API
            // methods already use, rather than a bespoke channel.
            // ADR-0082 §11 / issue 1378: the frame graph is `Tick →
            // Render → Tick`, so one requested tick drives a full
            // two-stage cycle. Each iteration pushes one `LifecycleAdvance`
            // (broadcasting the cap's current stage) and blocks on its
            // `LifecycleAdvanceComplete` reply, reading `next` to learn the
            // cap's resolved next stage; the loop exits once it returns to
            // `Tick` (cycle complete) or reaches a terminal (`next == 0`).
            // The reply gate is exactly the #999 fix below — emitted only
            // after the cap clears `pending`, so the next iteration's
            // advance never races the overlap guard.
            loop {
                let cid = self.fresh_correlation_id();
                // Mint a chassis-root `LifecycleAdvance` (so the trace
                // pipeline tracks the broadcast subtree and `on_settled`
                // fires) that *also* carries this bench's session as the
                // reply target — the driver routes `LifecycleAdvanceComplete`
                // there via `on_settled`'s `ctx.reply_to`. `push_chassis_root_mail`
                // doesn't take a reply target, so the chassis-root push is
                // open-coded here (mint id → record `Sent` → push with both
                // lineage and reply-to), mirroring its three steps.
                let advance_root =
                    MailId::new(MailboxId::CHASSIS_MAILBOX_ID, self.fresh_correlation_id());
                self.queue.record_sent(
                    advance_root,
                    advance_root,
                    None,
                    MailboxId::CHASSIS_MAILBOX_ID,
                    self.lifecycle_mailbox,
                    self.kind_lifecycle_advance,
                );
                let reply_to = Source::with_correlation(SourceAddr::Session(self.session), cid);
                self.queue.push(
                    Mail::new(
                        self.lifecycle_mailbox,
                        self.kind_lifecycle_advance,
                        encode_empty::<aether_kinds::LifecycleAdvance>(),
                        1,
                    )
                    .with_lineage(advance_root, advance_root, None)
                    .with_reply_to(reply_to),
                );
                // Block until the driver replies `LifecycleAdvanceComplete`
                // for this advance. A `Timeout` here means the chain never
                // settled (a genuine in_flight leak in some downstream cap)
                // or the driver never replied — same fail-loud disposition
                // the prior `SettlementTimeout` had.
                let complete = self.pump_until_reply::<aether_kinds::LifecycleAdvanceComplete>(
                    cid,
                    "LifecycleAdvanceComplete",
                )?;
                if complete.next == <Tick as Kind>::ID.0 || complete.next == 0 {
                    break;
                }
            }
        }
        // ADR-0082 §6 / PR 3c: the advance settlement above already
        // waited for the whole frame chain (Tick → component →
        // DrawTriangle → render cap accumulator) to drain, so render's
        // inbox is quiesced by the time we reach submit. The prior
        // `drain_frame_bound_or_abort` pending-counter poll is
        // redundant under settlement gating and retired.
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
                    if let WaitOutcome::Wedged(_) = await_internal_signal(
                        &rx,
                        "test_bench.capture_pre_mail",
                        SETTLEMENT_TIMEOUT,
                        self.settlement_cap,
                        TerminalDisposition::ReplyErr,
                    ) {
                        return Err(self.settlement_timeout(
                            "capture pre-mail chain".to_owned(),
                            "<pre-mail>",
                            "test_bench.capture_pre_mail",
                        ));
                    }
                }
                let result = CaptureFrameResult::from(
                    self.gpu
                        .render_and_capture(&req.checks, req.reference.as_ref()),
                );
                for mail in req.after_mails {
                    self.queue.push(mail);
                }
                // Reply through the retained inbound guard (ADR-0106 /
                // #1758). The bench's capture reply target is a `Session`,
                // so the guard's `reply` routes through the same `outbound`
                // egress `send_reply` did (the `RecordingBackend` loopback
                // picks it up by correlation).
                req.reply.reply(&result);
                // iamacoffeepot/aether#1273 / #1758: `req` still owns
                // `req.reply` after the partial moves above; the retained
                // inbound guard drops at end of this match arm — *after*
                // `reply` returns — so the inbound's `Finished` records
                // after the reply's `Sent` (ADR-0080 §6). Don't restructure
                // to move the reply below other work in this arm.
            }
            None => {
                self.gpu.render();
            }
        }

        Ok(())
    }
}

/// Render the settlement table's pending roots as a compact wedge
/// diagnostic — `root → in_flight=N held_open=M`, comma-joined (issue
/// 2062). Empty renders `<none>`: a wedge with nothing pending points at
/// the signal wiring (a dropped subscriber, a lost `Settled`), not a
/// stuck chain.
fn format_pending_roots(pending: &[(MailId, u32, u32)]) -> String {
    if pending.is_empty() {
        return "<none>".to_owned();
    }
    pending
        .iter()
        .map(|(root, in_flight, held_open)| {
            format!("{root:?} → in_flight={in_flight} held_open={held_open}")
        })
        .collect::<Vec<_>>()
        .join(", ")
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
// Tests assert spawned-child ids against the name hash — the primitive is
// the reference value under test, not sibling-cap addressing.
#[allow(clippy::disallowed_methods)]
#[allow(clippy::too_many_lines, clippy::significant_drop_tightening)]
mod tests {
    use super::*;

    /// The wedge dump renders each pending root with its counts (issue
    /// 2062) — a pure-function check, no chassis boot needed.
    #[test]
    fn format_pending_roots_renders_counts() {
        let a = MailId {
            sender: MailboxId(1),
            correlation_id: 2,
        };
        let rendered = format_pending_roots(&[(a, 3, 1)]);
        assert!(rendered.contains("in_flight=3"), "rendered: {rendered}");
        assert!(rendered.contains("held_open=1"), "rendered: {rendered}");
    }

    /// An empty pending set renders `<none>` rather than a blank string,
    /// so a wedge with nothing pending reads as a signal-wiring fault, not
    /// a stuck chain.
    #[test]
    fn format_pending_roots_empty_is_none() {
        assert_eq!(format_pending_roots(&[]), "<none>");
    }

    use crate::test_bench::BenchOp;
    use std::thread;
    use std::time::Instant;

    /// Issue 2062: a wedged settlement gate names the stuck root and its
    /// `(in_flight, held_open)` counts instead of a bare timeout. Drive
    /// the dump path directly against a deliberately-stuck root recorded
    /// on the live settlement table — `record_sent` with no matching
    /// `Finished` leaves a root at `in_flight=1` forever — and assert the
    /// surfaced `SettlementTimeout` enumerates it. No timing wait: the
    /// gate's *wedge detection* is covered by `await_internal_signal`'s
    /// own tests; this covers the *diagnostic content*.
    #[test]
    fn settlement_wedge_dump_names_stuck_root() {
        let tb = match TestBench::start_with_size(64, 48) {
            Ok(tb) => tb,
            Err(e) => {
                eprintln!("skipping: TestBench boot failed (likely no wgpu adapter): {e}");
                return;
            }
        };
        // A synthetic root that never settles: one `Sent`, no `Finished`.
        let stuck = MailId {
            sender: MailboxId(0xDEAD),
            correlation_id: 0xBEEF,
        };
        tb.queue
            .trace_handle()
            .settlement_counter()
            .record_sent(stuck);

        let err = tb.settlement_timeout("stuck.recipient".to_owned(), "StuckKind", "test.wedge");
        let TestBenchError::SettlementTimeout { pending, .. } = &err else {
            panic!("expected SettlementTimeout, got {err:?}");
        };
        assert!(
            pending.contains("in_flight=1"),
            "dump should name the stuck root with in_flight=1: {pending}",
        );
        // The rendered error string carries the dump too.
        assert!(
            err.to_string().contains("in_flight=1"),
            "Display should surface the pending dump: {err}",
        );
    }

    /// Boot, advance one tick, capture, sanity-check the PNG.
    /// The default scene is empty so the captured frame is the
    /// background-clear color uniformly. The test asserts the PNG
    /// is well-formed; deeper visual assertions land in the scenario
    /// library.
    ///
    /// The unit test lets `TestBench::start_with_size` fail naturally
    /// on driverless runners and skips on any boot error, rather than
    /// pulling in the `test_helpers` wgpu probe — keeping the lib
    /// unit test self-contained (the same skip semantics, keyed off
    /// the boot result rather than a separate adapter probe).
    #[test]
    fn boot_advance_capture_round_trip() {
        let mut tb = match TestBench::start_with_size(64, 48) {
            Ok(tb) => tb,
            Err(e) => {
                eprintln!("skipping: TestBench boot failed (likely no wgpu adapter): {e}");
                return;
            }
        };
        let result = tb
            .execute(vec![
                ("tick", BenchOp::advance(1)),
                ("snap", BenchOp::capture()),
            ])
            .expect("advance + capture");
        let png = result.captured("snap").expect("snap step ran");
        assert!(
            png.starts_with(&[0x89, 0x50, 0x4E, 0x47]),
            "captured bytes are not a PNG: first 8 bytes={:?}",
            &png.iter().take(8).copied().collect::<Vec<u8>>(),
        );
    }

    /// iamacoffeepot/aether#1273: `on_capture_frame` parks the request
    /// on the capture queue and returns immediately — the reply happens
    /// later on the chassis main thread. ADR-0086 §12 says deferred
    /// replies MUST hold-open against the trace root; without that hold
    /// `Settled{root}` fires before the reply lands and the wire `Call`
    /// driving the MCP tool ends with zero collected reply events.
    ///
    /// This test sends `CaptureFrame` via `BenchOp::send_and_await` (the
    /// shape the issue's regression test calls for) and asserts the
    /// reply decodes to `CaptureFrameResult::Ok { png: <non-empty> }`.
    /// The PNG comes back through the loopback's `EgressEvent::ToSession`
    /// — same correlation-id round-trip the MCP harness uses, but
    /// in-process.
    #[test]
    fn capture_frame_send_and_await_returns_png() {
        let mut tb = match TestBench::start_with_size(64, 48) {
            Ok(tb) => tb,
            Err(e) => {
                eprintln!("skipping: TestBench boot failed (likely no wgpu adapter): {e}");
                return;
            }
        };
        let result = tb
            .execute(vec![
                ("tick", BenchOp::advance(1)),
                (
                    "capture",
                    BenchOp::send_and_await(
                        RenderCapability::NAMESPACE,
                        &CaptureFrame {
                            mails: Vec::new(),
                            after_mails: Vec::new(),
                            checks: Vec::new(),
                            similarity: None,
                        },
                    ),
                ),
            ])
            .expect("advance + send_and_await(CaptureFrame)");
        let reply: CaptureFrameResult = result
            .reply("capture")
            .expect("capture step replied with CaptureFrameResult");
        match reply {
            CaptureFrameResult::Ok { png, verdict, .. } => {
                assert!(
                    verdict.is_none(),
                    "no checks were requested, so the verdict must be absent",
                );
                assert!(
                    png.starts_with(&[0x89, 0x50, 0x4E, 0x47]),
                    "captured bytes are not a PNG: first 8 bytes={:?}",
                    &png.iter().take(8).copied().collect::<Vec<u8>>(),
                );
            }
            CaptureFrameResult::Err { error } => {
                panic!("capture_frame replied Err: {error}");
            }
        }
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
        use aether_kinds::LifecycleSubscribe;
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

        // Subscribe the closure mailbox to the `Tick` lifecycle stage
        // (goes through the lifecycle cap's on_subscribe handler), then
        // advance one tick. The advance issues a `LifecycleAdvance` whose
        // chain root the lifecycle cap threads through
        // `broadcast_to_subscribers` (`send_envelope_traced`) to every
        // stage subscriber (issue 723 lineage, ADR-0082 §6). Sequencing
        // both through `execute` settles the subscribe before the tick
        // fires.
        tb.execute(vec![
            (
                "subscribe",
                BenchOp::send_mail(
                    "aether.lifecycle",
                    &LifecycleSubscribe {
                        stage: Tick::ID.0,
                        mailbox: subscriber_mbox.0,
                    },
                ),
            ),
            ("advance", BenchOp::advance(1)),
        ])
        .expect("subscribe + advance");

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

    /// iamacoffeepot/aether#1489: a `Quit` mail drives the frame
    /// lifecycle to its `Shutdown` terminal, finishing the in-flight
    /// `Tick → Render → Present` frame first because the quit edge lives
    /// on `Present` (ADR-0082 §3). This is the CI-runnable coverage for
    /// the drain — the desktop winit `CloseRequested` / ctrlc bridges
    /// that push the `Quit` are MCP-smoke territory, but the `Quit →
    /// Present → Shutdown` graph behaviour they depend on is exercised
    /// here without a live window (the bench shares the same
    /// `frame_lifecycle_config` graph desktop uses).
    ///
    /// Registers an inline mailbox subscribed to the `Shutdown` stage,
    /// sends `Quit` to `aether.lifecycle`, then advances one frame. The
    /// run-frame loop drives the whole `Tick → Render → Present →
    /// Shutdown` chain in that single advance once `quit_pending` is set
    /// (each stage breaks the loop only at `Tick` cycle-complete or the
    /// `next == 0` terminal), so observing the `Shutdown` broadcast at the
    /// subscriber proves the quit was consumed at `Present` and that
    /// `Shutdown` fired + settled.
    #[test]
    fn quit_drains_frame_then_broadcasts_shutdown() {
        use aether_data::Kind as DataKind;
        use aether_kinds::{LifecycleSubscribe, Quit, Shutdown};
        use aether_substrate::mail::registry::MailDispatch;
        use std::sync::Mutex;

        let mut tb = match TestBench::start_with_size(64, 48) {
            Ok(tb) => tb,
            Err(e) => {
                eprintln!("skipping: TestBench boot failed (likely no wgpu adapter): {e}");
                return;
            }
        };

        // Record the kind id of every mail the observer receives. The
        // lifecycle cap broadcasts the `Shutdown` stage to its
        // subscribers when it reaches the terminal; `register_inline` is
        // the right variant (immediate work, no downstream enqueue) so
        // the producer-side settlement bracket stays on the broadcast
        // call site — the same shape the Tick-fanout test above relies on.
        let observed: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        let observed_for_handler = Arc::clone(&observed);
        let observer_mailbox = tb.registry.register_inline(
            "issue_1489_shutdown_observer",
            Arc::new(move |dispatch: MailDispatch<'_>| {
                observed_for_handler
                    .lock()
                    .expect("test setup: observed mutex is never poisoned")
                    .push(dispatch.kind.0);
            }),
        );

        // Subscribe to Shutdown, set the quit flag, then advance one
        // frame — `execute` settles each step before the next, so the
        // subscription and `quit_pending` are both in place when the
        // advance fires.
        tb.execute(vec![
            (
                "subscribe_shutdown",
                BenchOp::send_mail(
                    "aether.lifecycle",
                    &LifecycleSubscribe {
                        stage: <Shutdown as DataKind>::ID.0,
                        mailbox: observer_mailbox.0,
                    },
                ),
            ),
            ("quit", BenchOp::send_mail("aether.lifecycle", &Quit {})),
            ("advance", BenchOp::advance(1)),
        ])
        .expect("subscribe + quit + advance");

        let observed = observed
            .lock()
            .expect("test setup: observed mutex is never poisoned");
        assert!(
            observed.contains(&<Shutdown as DataKind>::ID.0),
            "Shutdown broadcast never reached the subscriber — quit was not drained to the \
             terminal; observed kind ids: {observed:?}",
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
        use aether_actor::{Addressable as ActorTrait, HandlesKind};
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
            aether_data::pod_kind_codec!();
        }

        struct Child {
            received: Arc<AtomicU32>,
        }
        impl ActorTrait for Child {
            const NAMESPACE: &'static str = "test.spawn.child";
            type Resolver = aether_actor::Many;
        }
        impl HandlesKind<Bump> for Child {}
        impl aether_actor::Lifecycle for Child {
            type Config = Arc<AtomicU32>;
            type InitError = BootError;
            type InitCtx<'a> = NativeInitCtx<'a>;
            type Ctx<'a> = NativeCtx<'a>;
            fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self { received: config })
            }
        }
        impl NativeActor for Child {}
        impl NativeDispatch for Child {
            fn __aether_dispatch_envelope(
                &mut self,
                _ctx: &mut NativeCtx<'_, aether_substrate::Manual>,
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
