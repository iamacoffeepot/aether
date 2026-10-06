//! `SubstrateHarness` — the in-process driver for the substrate-harness chassis (ADR-0067).
//!
//! Boots the same substrate machinery `main.rs` does, but attaches a
//! recording backend to `outbound` through
//! [`HubOutbound::attach_recording`](aether_substrate::HubOutbound::attach_recording)
//! instead of relying on an external
//! egress target. Substrate-emitted replies arrive on `loopback_rx`
//! as [`EgressEvent`]s so the test thread can correlate them to its
//! requests by `correlation_id`.
//!
//! The chassis-control handler pushes `Advance` events onto the events channel. `SubstrateHarness::advance`
//! drains the queue (which lets the handler run), pumps any pending events
//! through `run_frame` synchronously, then drains the loopback for the
//! matching reply. Capture is mail-driven inside the pumped render actor;
//! the pump loop drives it.
//!
//! Reply correlation: every API call gets a fresh `correlation_id`.
//! The substrate echoes it on the reply per ADR-0042, so multiple
//! in-flight requests are unambiguous. The common `execute` path stays
//! synchronous, while focused tests can defer replies explicitly.

// Test-only skip diagnostics emit `eprintln!` so `cargo test` runners
// surface a visible "skipping: ..." line alongside `test ... ok`;
// not routed through `tracing` (issue 891).
#![cfg_attr(test, allow(clippy::print_stderr))]

use std::any::type_name;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use aether_component::ComponentHostCapability;
use aether_data::{ErasedActorPath, Kind, KindId, LoadName, ReplyContract, SessionToken, Uuid};
#[cfg(test)]
use aether_kinds::trace::{DescribeTreeResult, TraceTail, TraceTailResult};
use aether_kinds::{Advance, AdvanceResult, CaptureFrame, CaptureFrameResult, CostTail, CostTailResult};
use aether_kinds::{LogTail, LogTailResult, Tick};
#[cfg(test)]
use aether_trace::walk::TreeWalk;
// The driver sends encode each kind through the descriptor-aware
// `Kind::encode_into_bytes` (cast or structured per the kind's shape).
use aether_actor::{ActorRef, Addressable, CastTarget, ChildOf, ErasedActorRef, Instanced, ProtocolRef, Root};
use aether_fs::NamespaceRoots;
use aether_substrate::PumpedSlot;
use aether_substrate::chassis::ctx::MailboxWakeFn;
use aether_substrate::chassis::settlement::{
    PumpWake, TerminalDisposition, WaitOutcome, await_internal_signal, await_settlement_pumped,
};
use aether_substrate::config::{ConfigMember, SettlementConfig};
#[cfg(test)]
use aether_substrate::mail::MailboxId;
use aether_substrate::{
    Builder, ChassisTarget, ChildRefused, EgressEvent, NativeActor, PassiveChassis, ReplyTarget, RingCapacities,
    RouteReadProbe, SchedulerTuning, SubstrateBoot, mail::MailId,
};

use crate::{PreparedSend, SendTarget};
use aether_substrate_harness_cap::SubstrateHarnessCapability;

use super::chassis::{
    ComponentHostMode, ComposeFn, FrameHook, RenderHookWiring, SubstrateHarnessBuild, SubstrateHarnessChassis,
    SubstrateHarnessEnv, WORKERS,
};
use aether_substrate_harness_cap::events::{ChassisEvent, EventReceiver, channel as event_channel};
use std::error;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

/// A reply source's wake: sends [`PumpWake::Mail`] on the harness's one wake
/// channel. The source fires it after it enqueues, so the pump loop woken by
/// it finds the item.
fn mail_wake(wake: &Sender<PumpWake>) -> MailboxWakeFn {
    let wake = wake.clone();
    Arc::new(move || {
        let _ = wake.send(PumpWake::Mail);
    })
}

/// Boxed [`FrameHook`] constructor the render extension registers on the
/// builder: runs in the build's start, against the live passive (to boot the
/// reserved pumped render slot via `PassiveChassis::boot_pumped_actor`), the
/// render wiring, and the builder's offscreen size (ADR-0161 slice R4). Not
/// `Send`: it constructs the `!Send` pumped slot on the harness thread.
pub type HookFactory = Box<
    dyn FnOnce(
        &PassiveChassis<SubstrateHarnessChassis>,
        RenderHookWiring,
        u32,
        u32,
    ) -> anyhow::Result<Box<dyn FrameHook>>,
>;

/// Default offscreen target dimensions when the caller picks
/// `start()` (no explicit size). 800x600 matches the scenario harness
/// convention — large enough that `min_non_bg_pixels` thresholds
/// discriminate, small enough that capture readback is cheap.
pub const DEFAULT_WIDTH: u32 = 800;
pub const DEFAULT_HEIGHT: u32 = 600;

/// Errors `SubstrateHarness` API methods surface. `Boot` covers any failure
/// in the substrate's `build()`; `Decode` covers structured reply
/// decode failures (rare — implies a kind shape mismatch); `Timeout`
/// covers replies that never arrive (chassis hung or wrong target);
/// `Advance` and `Capture` pass through `Err` variants from the
/// substrate's reply. `ChildRefused` surfaces when
/// [`SubstrateHarness::child`] finds no live child at the key it was asked
/// for. `SettlementTimeout` surfaces when a
/// `send_and_settle` chain didn't settle before the
/// settlement-patience backstop (issue 834: the harness waits on each
/// pushed chain's `Settled { root }` so the next observation —
/// `capture()`, the next typed send, an assertion — is causally
/// after the producer's full descendant tree dispatched). Issue 2062:
/// the backstop is a generous deadlock/livelock cap a healthy chain
/// never reaches, so a `SettlementTimeout` names a genuine wedge and
/// carries a `pending` dump of the stuck roots and their counts.
#[derive(Debug)]
pub enum SubstrateHarnessError {
    Boot(String),
    Decode(String),
    Timeout {
        expected: &'static str,
        pumped_iterations: u32,
    },
    Advance(String),
    Capture(String),
    /// [`SubstrateHarness::child`] found no `Live` child at the key it was
    /// asked for: never spawned, still starting, or already dropped.
    ChildRefused(ChildRefused),
    /// A component load the harness drove itself ([`SubstrateHarness::load`])
    /// was refused, or its reply could not be adopted as the loaded actor.
    Load(String),
    /// A publish the harness drove itself ([`SubstrateHarness::publish`]) was
    /// refused.
    Publish(String),
    /// A spawn the harness drove itself ([`SubstrateHarness::spawn_any`] and
    /// the typed spawns) was refused, or its reply could not be adopted as the
    /// spawned actor.
    Spawn(String),
    /// [`SubstrateHarness::cast`] found the reference's route not `Live`, or
    /// its published rows do not answer `protocol`. `path` is the canonical
    /// path the registry retains for the reference, when it retains one.
    CastRefused {
        /// The protocol's type name.
        protocol: &'static str,
        path: Option<ErasedActorPath>,
    },
    SettlementTimeout {
        /// The sent mail's kind, as the registry labels it.
        kind: String,
        /// Diagnostic dump of the settlement table's pending roots at the
        /// moment the gate wedged — `root → in_flight=N held_open=M`,
        /// comma-joined (or `<none>`). Names the stuck chain so a genuine
        /// deadlock/livelock is actionable, not a bare timeout (issue 2062).
        pending: String,
    },
}

impl fmt::Display for SubstrateHarnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Boot(e) => write!(f, "substrate boot failed: {e}"),
            Self::Decode(e) => write!(f, "decode reply: {e}"),
            Self::Timeout { expected, pumped_iterations } => {
                write!(f, "expected {expected} reply, did not arrive within {pumped_iterations} pump iterations")
            }
            Self::Advance(e) => write!(f, "advance failed: {e}"),
            Self::Capture(e) => write!(f, "capture failed: {e}"),
            Self::ChildRefused(refused) => write!(f, "child lookup refused: {refused}"),
            Self::Load(e) => write!(f, "component load failed: {e}"),
            Self::Publish(e) => write!(f, "component publish failed: {e}"),
            Self::Spawn(e) => write!(f, "component spawn failed: {e}"),
            Self::CastRefused { protocol, path: Some(path) } => {
                write!(f, "the actor at {path} does not answer the protocol {protocol}")
            }
            Self::CastRefused { protocol, path: None } => {
                write!(f, "a reference with no retained path does not answer the protocol {protocol}")
            }
            Self::SettlementTimeout { kind, pending } => write!(
                f,
                "send of {kind} did not settle before the patience backstop — a genuine deadlock/livelock in the chain (a healthy chain never reaches this cap); pending roots: {pending}",
            ),
        }
    }
}

/// Per-round settlement patience: the re-arm interval of the escalating
/// wait, i.e. how often
/// [`aether_substrate::chassis::settlement::await_internal_signal`] logs `gate … slow …
/// extending` while a slow-but-healthy chain is still settling. The log
/// heartbeat, not the gate — a chain that settles is unaffected by its
/// value; only the backstop cap (see [`SubstrateHarness::settlement_cap`])
/// declares a wedge. Long enough to absorb wasm compile + cap dispatcher
/// wake under nextest CPU contention.
const SETTLEMENT_TIMEOUT: Duration = Duration::from_secs(5);

impl error::Error for SubstrateHarnessError {}

/// In-process substrate-harness driver. Owns the substrate, runs the
/// chassis events loop synchronously inside its API methods, routes
/// substrate replies through a loopback channel.
///
/// Construction boots a fresh substrate and attaches the loopback;
/// drop tears it down (the held `_boot` is the lifetime guard for
/// the scheduler workers). Methods are `&mut self` because they
/// mutate frame state and pump events; concurrent calls are not
/// supported.
pub struct SubstrateHarness {
    loopback_rx: mpsc::Receiver<EgressEvent>,

    events_rx: EventReceiver,

    /// The one wake channel every reply source signals (ADR-0161 §Decision
    /// 2): the loopback recorder and the event channel after each enqueue,
    /// the pumped render slot after each accepted mail, and a render settle's
    /// subscription on settlement. [`Self::pump_until_event`] blocks here
    /// instead of sleeping, and it alone empties the queue inside a pump
    /// wait — always before it drains the sources, so no wake is lost.
    wake_rx: Receiver<PumpWake>,
    /// A sender on the same channel, for the settlement subscriptions a
    /// pumped component host's settle wait takes.
    wake_tx: Sender<PumpWake>,

    /// The `aether.lifecycle` capability's proven reference, read off the
    /// chassis's composed record at boot. `advance()` fires one
    /// `LifecycleAdvance` here per requested tick; the lifecycle driver
    /// broadcasts the `Tick` stage directly to its stage subscriber set per
    /// ADR-0082.
    lifecycle: ActorRef<aether_lifecycle::LifecycleCapability>,
    frame: u64,
    /// Counter behind [`Self::fresh_correlation_id`]: session-reply
    /// correlations only, never chassis roots.
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

    /// Kind ids of the reports fixtures mail to the harness observer
    /// inbox (`aether.substrate_harness.observer`), such as
    /// `aether.test_fixture.boot_observed`. Read back via
    /// [`Self::count_observed`] / [`Self::observed_kinds`] for scenario
    /// assertions. Only mail addressed to the observer lands here; a
    /// capability's own dispatches do not, so a test asserts what its work
    /// produced (a reply, a committed-frame snapshot, pixels) instead.
    observed_kinds: Arc<Mutex<Vec<KindId>>>,

    /// Lifetime guard. Boot owns the scheduler; dropping the
    /// `SubstrateHarness` drops the boot which joins the worker threads.
    /// Only the `#[cfg(test)]` fixtures read it, through `Self::boot`.
    _boot: SubstrateBoot,

    /// `PassiveChassis<SubstrateHarnessChassis>` holding the booted
    /// passives via the `chassis_builder` typed map. Held for the harness's
    /// lifetime so the passives' dispatchers stay alive. Fields drop in
    /// declaration order, and this one is declared before the two pumped
    /// roots below, so the chassis tears down first: its instanced actors
    /// and composed roots close while the pumped roots they depend on are
    /// still open (ADR-0160 §3).
    pub(crate) passive: PassiveChassis<SubstrateHarnessChassis>,

    /// The pumped component host, when the builder asked for one. Declared
    /// after `passive`, so it closes, in its slot's drop, once the guests it
    /// hosted are gone.
    component_host: Option<PumpedHost>,

    /// Frame-pump render seam, `Some` iff the builder registered a
    /// render extension (issues #3764/#3765). Captures require it; an
    /// advance without one skips the per-frame draw. ADR-0161 slice R4:
    /// the hook owns the pumped `aether.render` slot. Declared last, so
    /// render closes after everything that draws through it, and mail a
    /// closing actor left on its inbox is dispatched by its close.
    hook: Option<Box<dyn FrameHook>>,
}

/// The pumped component host's slot, which closes when the harness drops
/// it.
struct PumpedHost(PumpedSlot<ComponentHostCapability>);

/// A request enqueued through [`SubstrateHarness::send_deferred`] whose reply will
/// be awaited later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingBenchReply {
    cid: u64,
    expected: &'static str,
}

/// Fixed UUID used as the `SessionToken` for in-process replies.
/// Any non-zero literal works — the substrate just echoes whatever
/// it's handed in `SourceAddr::Session`. Spelled out as a constant
/// so the boot path is reproducible and the value shows up in logs.
const TESTBENCH_SESSION_UUID: u128 = 0x7E57_BE7C_C0FF_EE15_AE7E_7BE7_5E55_1077;

/// Builder for [`SubstrateHarness`]. Holds the optional config a test wants
/// to override (offscreen target size, ADR-0041 namespace roots).
/// Tests that want full default behaviour skip the builder and call
/// [`SubstrateHarness::start`] / [`SubstrateHarness::start_with_size`] directly.
///
/// Per issue 464, the `namespace_roots` override lets a test redirect
/// `save://` / `assets://` / `config://` at a tempdir without touching
/// process env. Pair with `tempfile::TempDir` to scope the redirect to
/// a single test.
pub struct SubstrateHarnessBuilder {
    width: u32,
    height: u32,
    namespace_roots: Option<NamespaceRoots>,
    pool_workers: Option<usize>,
    log_ring_capacity: Option<usize>,
    trace_ring_capacity: Option<usize>,
    trace_ring_max_capacity: Option<usize>,
    settlement_cap: Option<Duration>,
    render_hook: Option<HookFactory>,
    component_host: ComponentHostMode,
    compose: Vec<ComposeFn>,
    scheduler_tuning: SchedulerTuning,
}

impl Default for SubstrateHarnessBuilder {
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
            render_hook: None,
            component_host: ComponentHostMode::Absent,
            compose: Vec::new(),
            scheduler_tuning: SchedulerTuning::default(),
        }
    }
}

impl SubstrateHarnessBuilder {
    /// Pin the scheduler's hot-path tuning for this harness.
    ///
    /// The scheduler reads these values off the resolved [`SchedulerTuning`]
    /// it is booted with; it performs no env reads of its own (issue 464). A
    /// chassis binary fills that struct from `aether-chassis`'s
    /// `SchedulerTuningConfig` — argv > env > file > default — but the harness
    /// resolves off a **hermetic** source stack (ADR-0156 §5) and cannot
    /// depend on `aether-chassis` regardless, since `aether-chassis` already
    /// depends on this crate. So the `AETHER_SPIN_WINDOW_USEC` /
    /// `AETHER_LOCAL_TIME_BUDGET_US` / … keys do nothing under a harness
    /// scenario, and this is the staging path that does: an explicit value on
    /// the programmatic layer, the same seam `with_actor_configured` uses.
    ///
    /// A scenario that wants to A/B a scheduler knob calls this. Setting the
    /// env key instead measures the same configuration twice (issue 4234).
    #[must_use]
    pub fn scheduler_tuning(mut self, tuning: SchedulerTuning) -> Self {
        self.scheduler_tuning = tuning;
        self
    }

    /// Set the offscreen target size. Width / height are clamped to a
    /// minimum of 1 inside `Gpu::new`.
    #[must_use]
    pub fn size(mut self, width: u32, height: u32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    /// Override the ADR-0041 namespace roots. Forwarded to the harness
    /// chassis's `aether.fs` roots at boot, so the
    /// `aether.fs` adapter wired by the harness resolves
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
    /// (`DEFAULT_RING_CAP`); `Some(n)` pins it. Per-harness, no process env
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
    /// `truncated_before`. Per-harness, no process env.
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
    /// ring absorbing a burst past its floor up to `n`. Per-harness, no
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
    /// the "no cap" sentinel. Per-harness, no process env.
    #[must_use]
    pub fn settlement_cap(mut self, cap: Option<Duration>) -> Self {
        self.settlement_cap = cap;
        self
    }

    /// Compose an arbitrary capability into this harness. The harness boots
    /// its basics (trace dispatch, inventory, the harness cap, lifecycle, synthetic
    /// window) and each scenario composes exactly the caps it
    /// needs on top (issue #3764); this is the generic surface for any
    /// cap without boot-internal wiring — `harness.with_actor::<TextCapability>(())`,
    /// a scenario-local `NativeActor`, and so on. Applied to the chassis builder
    /// in push order, between the harness basics and lifecycle.
    ///
    /// ADR-0156 §5: mirrors `Builder::with_actor` — it carries only `params`
    /// (the composer-supplied construction input). To also supply a cap's
    /// operator-resolvable `Config`, use the paired [`Self::with_actor_configured`].
    ///
    /// Parentless composition rejects a child-only actor at compile time:
    ///
    /// ```compile_fail,E0277
    /// use aether_actor::{Addressable, ChildOf, Lifecycle, Many, One};
    /// use aether_data::KindId;
    /// use aether_harness_substrate::SubstrateHarnessBuilder;
    /// use aether_substrate::{BootError, Dispatch, Unchecked, NativeActor, NativeCtx, NativeInitCtx};
    ///
    /// struct Parent;
    /// impl Addressable for Parent {
    ///     const NAMESPACE: &'static str = "example.parent";
    ///     type Resolver = One;
    /// }
    ///
    /// struct ChildOnly;
    /// impl Addressable for ChildOnly {
    ///     const NAMESPACE: &'static str = "example.child_only";
    ///     type Resolver = Many;
    /// }
    /// impl ChildOf<Parent> for ChildOnly {}
    /// impl Lifecycle<Self> for ChildOnly {
    ///     type Config = ();
    ///     type Params = ();
    ///     type InitError = BootError;
    ///     type InitCtx<'a> = NativeInitCtx<'a>;
    ///     type Ctx<'a> = NativeCtx<'a, Self>;
    ///
    ///     fn init(_: (), _: (), _: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
    ///         Ok(Self)
    ///     }
    /// }
    /// impl Dispatch<Self> for ChildOnly {
    ///     fn dispatch(
    ///         _: &mut Self,
    ///         _: &mut NativeCtx<'_, Self, Unchecked>,
    ///         _: KindId,
    ///         _: &[u8],
    ///     ) -> Option<()> {
    ///         None
    ///     }
    /// }
    /// impl aether_actor::Declared for ChildOnly {
    ///     type Depends = ();
    ///     type Spawns = ();
    ///     type Parents = ();
    /// }
    /// impl NativeActor for ChildOnly {
    ///     type State = Self;
    /// }
    ///
    /// let _ = SubstrateHarnessBuilder::default().with_actor::<ChildOnly>(());
    /// ```
    #[must_use]
    pub fn with_actor<A>(mut self, params: A::Params) -> Self
    where
        A: Root + NativeActor,
        A::Config: Send + 'static + ConfigMember,
        A::Params: Send + 'static,
    {
        self.compose.push(Box::new(move |builder| builder.with_actor::<A>(params)));
        self
    }

    /// ADR-0156 §5: the paired form — compose actor `A` and stage its explicit
    /// `config` in one compiler-checked call (mirrors `Builder::with_actor_configured`).
    /// This is the **primary** API for a scenario that composes an actor and
    /// supplies its config value together: the `A::Config` type binds `config`
    /// at the call, and the harness's hermetic source stack means an unstaged
    /// member falls through to its compiled default rather than process env.
    #[must_use]
    pub fn with_actor_configured<A>(mut self, params: A::Params, config: A::Config) -> Self
    where
        A: Root + NativeActor,
        A::Config: Send + 'static + ConfigMember,
        A::Params: Send + 'static,
    {
        self.compose.push(Box::new(move |builder| builder.with_actor_configured::<A>(params, config)));
        self
    }

    /// Register the render seam (ADR-0161 R5) for the pumped actor `A`: the
    /// chassis reserves `A`'s slot at the Claim stage, so a composed passive
    /// may depend on it, and `hook_factory` boots `A` from that reservation
    /// (via `PassiveChassis::boot_pumped_actor`) in the build's start, at the
    /// builder's offscreen size, and builds the frame-pump [`FrameHook`] that
    /// drains it. The capture crate's `RenderHarnessBuilderExt::with_render`
    /// is the intended caller; without a registration, captures reply `Err`
    /// and the advance path skips the per-frame draw.
    #[must_use]
    pub fn render_hook<A: Root + NativeActor>(mut self, hook_factory: HookFactory) -> Self {
        self.render_hook = Some(hook_factory);
        self.compose.push(Box::new(Builder::reserve_pumped::<A>));
        self
    }

    /// Compose the component host, the cap behind
    /// `aether.component.{load,replace,drop}`. Any scenario that loads
    /// wasm needs this.
    #[must_use]
    pub fn with_component_host(mut self) -> Self {
        self.component_host = ComponentHostMode::Pooled;
        self
    }

    /// Compose the component host as a pumped actor (ADR-0160): it
    /// dispatches only while this harness drains it. Every harness wait
    /// drains it as a chassis driver would, so loads, drops and replaces
    /// answer as they do on the pool; between waits the host holds still,
    /// and [`SubstrateHarness::step_component_host_through`] runs it one
    /// envelope at a time. A scenario holds a republish prepared this way
    /// (ADR-0241 §7). It composes without a render hook, whose slot the
    /// harness would otherwise have to drain in the same waits.
    #[must_use]
    pub fn with_pumped_component_host(mut self) -> Self {
        self.component_host = ComponentHostMode::Pumped;
        self
    }

    /// Boot the harness. Overrides applied via the builder methods flow
    /// through to `SubstrateBoot::build` and the chassis-side sink
    /// wiring; the composed cap set is exactly the basics plus what the
    /// builder chain added.
    pub fn build(self) -> Result<SubstrateHarness, SubstrateHarnessError> {
        SubstrateHarness::start_inner(self)
    }
}

impl SubstrateHarness {
    /// Begin a `SubstrateHarness` boot. Default size 800x600, no
    /// `NamespaceRoots` override — chained methods on the returned
    /// builder set those.
    #[must_use]
    pub fn builder() -> SubstrateHarnessBuilder {
        SubstrateHarnessBuilder::default()
    }

    /// Boot a basics-only `SubstrateHarness` at the default 800x600
    /// offscreen size. Compose caps via [`Self::builder`].
    pub fn start() -> Result<Self, SubstrateHarnessError> {
        Self::builder().build()
    }

    /// Boot a basics-only `SubstrateHarness` with a specific offscreen
    /// target size. Width / height are clamped to a minimum of 1 inside
    /// `Gpu::new` when render is composed.
    pub fn start_with_size(width: u32, height: u32) -> Result<Self, SubstrateHarnessError> {
        Self::builder().size(width, height).build()
    }

    /// Block until the registry owner has applied and published every
    /// batch submitted before this call — see
    /// [`PassiveChassis::await_registry_applied`] for the FIFO argument
    /// and its precondition: this proves only an effect the caller already
    /// knows, through a real ordering signal, was submitted before the
    /// call.
    ///
    /// # Panics
    /// Panics when the registry owner refuses the barrier batch or does
    /// not complete it within the settlement cap.
    #[cfg(feature = "test-support")]
    pub fn await_registry_applied(&self) {
        self.passive.await_registry_applied();
    }

    /// Tear the engine down and count the reports matching `kind_name` the
    /// observer inbox had received by the end of the teardown: what
    /// [`Self::count_observed`] would answer if it could be asked after the
    /// drop. A scenario reads what the engine's own teardown made actors do,
    /// such as a guest's `unwire`, through this.
    ///
    /// # Panics
    /// Panics if the `observed_kinds` mutex is poisoned — fail-fast per
    /// ADR-0063.
    #[must_use]
    pub fn close_and_count_observed(self, kind_name: &str) -> usize {
        let wanted = self.passive.kind_id(kind_name);
        let observed = Arc::clone(&self.observed_kinds);
        drop(self);

        let Some(wanted) = wanted else {
            return 0;
        };
        observed
            .lock()
            .expect("observed_kinds mutex is never poisoned (ADR-0063 fail-fast)")
            .iter()
            .filter(|kind| **kind == wanted)
            .count()
    }

    fn start_inner(builder: SubstrateHarnessBuilder) -> Result<Self, SubstrateHarnessError> {
        let SubstrateHarnessBuilder {
            width,
            height,
            namespace_roots,
            pool_workers,
            log_ring_capacity,
            trace_ring_capacity,
            trace_ring_max_capacity,
            settlement_cap,
            render_hook,
            component_host,
            compose,
            scheduler_tuning,
        } = builder;

        // Lower the per-field `Option` overrides onto the `Copy`
        // `RingCapacities`, defaulting each unset field to the
        // `aether-actor` const cap. The harness defaults the trace ceiling
        // to the floor (a fixed, non-growing ring) so eviction tests
        // that pin a small floor observe `truncated_before`
        // deterministically; growth is opt-in via
        // `trace_ring_max_capacity`. (Production chassis default the
        // ceiling to `DEFAULT_TRACE_RING_MAX_CAP` instead, via
        // `ActorRingConfig`.)
        let default = RingCapacities::default();
        let trace = trace_ring_capacity.unwrap_or(default.trace);
        let ring_capacities = RingCapacities {
            log: log_ring_capacity.unwrap_or(default.log),
            trace,
            trace_max: trace_ring_max_capacity.unwrap_or(trace),
        };
        let settlement_cap = settlement_cap.unwrap_or_else(|| SettlementConfig::from_env().to_cap());
        if component_host == ComponentHostMode::Pumped && render_hook.is_some() {
            return Err(SubstrateHarnessError::Boot(
                "a pumped component host composes without a render hook".to_owned(),
            ));
        }

        // The one wake channel the pump loop blocks on: the event channel,
        // the loopback recorder and the render slot each fire it after they
        // enqueue.
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded::<PumpWake>();
        let (events_tx, events_rx) = event_channel(mail_wake(&wake_tx));
        let observed_kinds = Arc::new(Mutex::new(Vec::<KindId>::new()));

        // ADR-0161 slice R4: the pumped render slot is booted in the build's
        // start by the hook factory, so the non-knob render wiring (the
        // similarity assets root) is handed to the factory rather than
        // composed through `RenderParams`. Resolve the assets root before
        // `namespace_roots` moves into the env, mirroring the chassis's own
        // capture-similarity wiring.
        let render_assets_dir = namespace_roots.as_ref().map(|roots| roots.assets.clone());

        // ADR-0071 phase 6: substrate boot + every cap goes through
        // `SubstrateHarnessChassis::build_passive`. Io is part of the chain when
        // `namespace_roots` is supplied and pre-validation passes;
        // the chassis warns and skips Io otherwise. Tests that care
        // about io supply tempdir roots through
        // `start_with_namespace_roots`; otherwise the harness skips Io.
        let env = SubstrateHarnessEnv {
            workers: WORKERS,
            pool_workers,
            ring_capacities,
            scheduler_tuning,
            observed_kinds: Some(Arc::clone(&observed_kinds)),
            events_tx,
            namespace_roots,
            component_host,
            compose,
            // Issue #2509: the teardown gate honors the same resolved cap
            // (env knob or programmatic override) as the settlement-await
            // loops the harness stores this value for.
            teardown_budget: settlement_cap,
            render_hook,
            render_size: (width, height),
            render_assets_dir,
            render_wake: wake_tx.clone(),
        };
        let SubstrateHarnessBuild { passive, boot, mut hook, component_host } =
            SubstrateHarnessChassis::build_passive(env).map_err(|e| SubstrateHarnessError::Boot(e.to_string()))?;
        let mut component_host = component_host.map(PumpedHost);

        // ADR-0160 / ADR-0161: the drain-at-pump-start rule — a pumped
        // driver whose loop starts parked drains once before its first real
        // pump so any mail queued during `init` / `wire` dispatches.
        if let Some(hook) = hook.as_mut() {
            hook.pump();
        }
        if let Some(PumpedHost(slot)) = component_host.as_mut() {
            slot.drain_available();
        }

        // Attach a recording backend to the boot's outbound. Replies
        // to this harness's session reach the outbound through the
        // mailer's reply path and arrive here as
        // `EgressEvent::ToSession`, which `pump_until_reply`
        // correlates by `correlation_id`.
        let loopback_rx = boot.outbound.attach_recording(Some(mail_wake(&wake_tx)));

        // The loopback driver's route to the lifecycle cap: the reference the
        // chassis recorded when it composed the cap.
        let lifecycle = passive.actor_ref::<aether_lifecycle::LifecycleCapability>();

        Ok(Self {
            loopback_rx,
            events_rx,
            wake_rx,
            hook,
            lifecycle,
            frame: 0,
            next_correlation_id: AtomicU64::new(1),
            settlement_cap,
            session: SessionToken(Uuid::from_u128(TESTBENCH_SESSION_UUID)),
            stashed_replies: HashMap::new(),
            observed_kinds,
            wake_tx,
            component_host,
            _boot: boot,
            passive,
        })
    }

    /// Count how many reports matching `kind_name` fixtures have mailed
    /// to the observer inbox. Only mail addressed to the observer is
    /// counted; a capability's dispatches, mail to other sinks, and direct
    /// component-to-component flows are not.
    ///
    /// # Panics
    /// Panics if the `observed_kinds` mutex is poisoned — fail-fast
    /// per ADR-0063: a poisoned mutex means a prior holder panicked
    /// under the guard.
    pub fn count_observed(&self, kind_name: &str) -> usize {
        // One name-to-id resolution per assertion, against a recorder that
        // stores ids. An unregistered name matches nothing, which is the same
        // answer the name-comparing version gave.
        let Some(wanted) = self.passive.kind_id(kind_name) else {
            return 0;
        };
        self.observed_kinds
            .lock()
            .expect("observed_kinds mutex is never poisoned (ADR-0063 fail-fast)")
            .iter()
            .filter(|kind| **kind == wanted)
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
            .iter()
            .map(|kind| self.passive.kind_label(*kind))
            .collect()
    }

    /// Best-effort observed-kind snapshot for failure diagnostics. Unlike the
    /// assertion-facing [`Self::observed_kinds`], a poisoned recorder must not
    /// obscure the execution error that triggered evidence retention.
    pub(super) fn diagnostic_observed_kinds(&self) -> Vec<String> {
        self.observed_kinds
            .lock()
            .map(|kinds| kinds.iter().map(|kind| self.passive.kind_label(*kind)).collect())
            .unwrap_or_default()
    }

    /// Borrow the registered frame hook, if any. The capture crate's
    /// `RenderHarnessExt` reaches its concrete `GpuFrameHook` through this
    /// (via [`FrameHook::as_any`]) for render-typed accessors like the
    /// committed-overlay snapshot (issue #3765).
    #[must_use]
    pub fn frame_hook(&self) -> Option<&dyn FrameHook> {
        self.hook.as_deref()
    }

    /// Tail the per-actor log ring (ADR-0081) of the actor `to` proves.
    /// Mirrors `FleetHarness::log_tail` over the harness's reply wait, so
    /// in-process scenario tests can assert guest-emitted
    /// `tracing::warn!` / `tracing::info!` entries without an RPC session.
    ///
    /// `since: None` reads from the oldest retained entry; `Some(n)` returns
    /// only entries with `sequence > n`. `contains` applies a case-sensitive
    /// message substring filter substrate-side. `max: 0` resolves to the
    /// substrate-default cap (currently 100). The framework dispatch loop
    /// answers [`LogTail`] for every native actor and wasm trampoline, so
    /// `to` is any typed reference — an `&ActorRef<R>` or a
    /// `&ProtocolRef<P>` — through [`SendTarget`]'s framework-tail arm,
    /// whether or not its type declares a [`LogTail`] handler.
    ///
    /// # Panics
    /// Panics on a decode failure — implies a kind shape mismatch,
    /// matching the fail-fast disposition of [`Self::count_observed`] /
    /// [`Self::observed_kinds`].
    pub fn log_tail<I>(
        &mut self,
        to: impl SendTarget<LogTail, I>,
        since: Option<u64>,
        contains: Option<String>,
    ) -> LogTailResult {
        let payload = self
            .request_prepared(&to.prepare(&LogTail { max: 0, min_level: None, since, contains }))
            .unwrap_or_else(|e| panic!("log_tail send failed: {e}"));
        LogTailResult::decode_from_bytes(&payload)
            .unwrap_or_else(|| panic!("log_tail reply did not decode as LogTailResult"))
    }

    /// The proven reference of the root actor `R` this harness composed — a
    /// basic, a capability a builder chain added, or the pumped render actor
    /// (ADR-0230 §3). Forwards to the chassis handle's `actor_ref`, which
    /// reads the reference the boot recorded and mints nothing.
    ///
    /// # Panics
    ///
    /// Panics naming `R::NAMESPACE` when this harness composed no `R`.
    #[must_use]
    pub fn actor_ref<R: Root + 'static>(&self) -> ActorRef<R> {
        self.passive.actor_ref::<R>()
    }

    /// The proven reference of the `C` instance keyed by `key` directly
    /// beneath `parent` — a child a loaded component spawned, or a window a
    /// window capability opened (ADR-0230 §3's child-beneath-a-held-reference
    /// door). The parent travels as a reference the harness already handed
    /// out, so a child is reached by type and key rather than by rendering
    /// its address.
    ///
    /// # Errors
    ///
    /// [`SubstrateHarnessError::ChildRefused`], naming the key and
    /// `C::NAMESPACE`, when no `Live` child stands at `key`.
    pub fn child<P, C>(&self, parent: &ActorRef<P>, key: LoadName) -> Result<ActorRef<C>, SubstrateHarnessError>
    where
        P: Addressable,
        C: ChildOf<P> + Instanced,
    {
        self.passive.child::<P, C>(*parent, key).map_err(SubstrateHarnessError::ChildRefused)
    }

    /// `reference`'s canonical lineage path, read from the registry (ADR-0230
    /// §2): a readable name for diagnostics and capture recipients, never a
    /// sendable address. The typed component doors — [`Self::load`],
    /// [`Self::spawn`], [`Self::spawn_keyed`], [`Self::spawn_child`] — prove
    /// only the reference; call this when a test needs the path too.
    ///
    /// # Panics
    ///
    /// Panics when the registry retains no path for `reference`. A minted
    /// reference always names a route whose proven canonical name the
    /// registry keeps for the session, so this should never fire in practice.
    #[must_use]
    pub fn actor_path<R: Addressable>(&self, reference: &ActorRef<R>) -> ErasedActorPath {
        self.passive
            .actor_path((*reference).erase())
            .expect("a minted reference names a route whose proven canonical name the registry keeps for the session")
    }

    /// Whether `actor` would dispatch `kind`: a declared handler or a `#[fallback]` (ADR-0033),
    /// as the capability registry reflects after load / replace / drop.
    /// Consumers: `aether-substrate/tests/cap_registry.rs`.
    #[must_use]
    pub fn accepts(&self, actor: ErasedActorRef, kind: KindId) -> bool {
        self.passive.accepts(actor, kind)
    }

    /// The contract `actor`'s route publishes (ADR-0231 §4): its `(KindId,
    /// ReplyContract)` rows sorted by kind, and whether it has a
    /// `#[fallback]`. `None` while the route does not resolve `Live`.
    /// Consumers: `aether-component/tests/harness_published_contract.rs`.
    #[must_use]
    pub fn published_contract(&self, actor: ErasedActorRef) -> Option<(Vec<(KindId, ReplyContract)>, bool)> {
        self.passive.published_contract(actor)
    }

    /// `actor`'s per-handler cost rows (ADR-0036), what the `actor_cost` MCP tool reports.
    /// Consumers: `aether-substrate/tests/cost_table.rs`, `aether-render/tests/actor_draw_cost_scenario.rs`.
    #[must_use]
    pub fn actor_cost(&self, actor: ErasedActorRef) -> CostTailResult {
        self.passive.actor_cost(actor, &CostTail { kind: None })
    }

    /// Type the erased reference `actor` as the protocol `P` (ADR-0231 §4's
    /// guard cast): the registry reads the route's `Live` published rows and
    /// mints a reference only when they answer every row of `P` with the exact
    /// reply. A wasm-only fixture from [`Self::load_any`] is sent to this way,
    /// against a test-local `#[protocol]`.
    ///
    /// # Errors
    ///
    /// [`SubstrateHarnessError::CastRefused`], naming `P` and the reference's
    /// retained path, when the route is not `Live` or its rows do not answer
    /// `P`.
    pub fn cast<P: CastTarget>(&self, actor: ErasedActorRef) -> Result<ProtocolRef<P>, SubstrateHarnessError> {
        self.passive.cast::<P>(actor).ok_or_else(|| SubstrateHarnessError::CastRefused {
            protocol: type_name::<P>(),
            path: self.passive.actor_path(actor),
        })
    }

    /// Bytes-level settlement-gated send: push `(kind, bytes)` to the actor
    /// `to` proves as a chassis-root mail and block until the dispatched
    /// chain settles (ADR-0080 §6). Backs the `SendAndSettle` op of
    /// [`Self::execute`].
    ///
    /// Issue 834: synchronous-on-settle. The mail is pushed as a
    /// chassis-root through [`PassiveChassis::send_tracked`] so the trace
    /// pipeline tracks the chain; the harness waits on the returned
    /// `Settled { root }` receiver for the chain (the recipient's handler +
    /// every descendant mail it spawned) to drain. By the time this
    /// returns, any subsequent observation is causally after the producer's
    /// full chain — no nudge_tick-style band-aids needed for render-flush
    /// races.
    ///
    /// ADR-0161 §Decision 2: this is a settlement wait that can include a
    /// render-recipient chain (a `send_and_settle(DrawTriangle / DestroyTexture /
    /// …)` addressed to `aether.render`, or one whose descendants reach it),
    /// so with a render hook it must drain the pumped render slot while
    /// waiting — the chain settles only because that drain runs. The hook's
    /// [`FrameHook::settle`] waits in `await_settlement_pumped`, the wait the
    /// drivers use, draining the slot on its mail wake and returning on the
    /// root's settlement; there is no fixed drain round. Without a hook the
    /// wait is `await_internal_signal` on the settlement receiver. Returns
    /// `SettlementTimeout` if the chain doesn't settle within the settlement
    /// cap.
    pub(crate) fn settle_prepared(&mut self, send: &PreparedSend, kind: KindId) -> Result<(), SubstrateHarnessError> {
        let (root, rx) = send
            .tracked(&self.passive, None)
            .map_err(|error| SubstrateHarnessError::Decode(format!("prepare harness send: {error}")))?;
        self.await_settlement(kind, root, &rx)
    }

    pub(crate) fn settle_bytes<K: Kind, I>(
        &mut self,
        to: impl ChassisTarget<K, I>,
        mail: &K,
    ) -> Result<(), SubstrateHarnessError> {
        let (root, rx) = self.passive.send_tracked(to, mail, None);
        self.await_settlement(K::ID, root, &rx)
    }

    pub(crate) fn await_settlement(
        &mut self,
        kind: KindId,
        root: MailId,
        rx: &Receiver<()>,
    ) -> Result<(), SubstrateHarnessError> {
        let gate = "substrate_harness.push_and_settle";
        let outcome = match (self.hook.as_mut(), self.component_host.as_mut()) {
            (Some(hook), _) => {
                hook.settle(self.passive.settlement_registry(), root, self.settlement_cap, &self.wake_rx)
            }
            // A chain through the pumped host settles only while its slot is
            // drained, so the wait drains it on each mail wake, as a pumped
            // driver does.
            (None, Some(PumpedHost(slot))) => {
                let wake = self.wake_tx.clone();
                self.passive.settlement_registry().subscribe_settlement_with(root, move || {
                    let _ = wake.send(PumpWake::Settled);
                });
                // Mail that reached the host before this wait fired a wake an
                // earlier wait may have consumed, so drain once up front.
                slot.drain_available();
                await_settlement_pumped(
                    &self.wake_rx,
                    slot,
                    gate,
                    SETTLEMENT_TIMEOUT,
                    self.settlement_cap,
                    TerminalDisposition::ReplyErr,
                )
            }
            (None, None) => await_internal_signal(
                rx,
                gate,
                SETTLEMENT_TIMEOUT,
                self.settlement_cap,
                TerminalDisposition::ReplyErr,
                None,
            ),
        };
        match outcome {
            WaitOutcome::Settled => Ok(()),
            WaitOutcome::Wedged(_) => Err(self.settlement_timeout(self.kind_label(kind), gate)),
        }
    }

    /// The registry's label for `kind` — its registered name, or a tagged
    /// id when none is registered. Names a sent kind in a harness failure.
    pub(crate) fn kind_label(&self, kind: KindId) -> String {
        self.passive.kind_label(kind)
    }

    /// Build a [`SubstrateHarnessError::SettlementTimeout`] carrying a dump of the
    /// settlement table's currently-pending roots, and log it (issue 2062).
    /// Shared by the settlement gate sites so a wedge — a genuine
    /// deadlock/livelock, since the cap is a generous backstop a healthy
    /// chain never reaches — names the stuck root(s) and their
    /// `(in_flight, held_open)` counts instead of surfacing a bare timeout.
    fn settlement_timeout(&self, kind: String, gate: &str) -> SubstrateHarnessError {
        let pending = format_pending_roots(&self.passive.pending_settlement_roots());
        tracing::error!(
            target: "aether_substrate::substrate_harness",
            gate,
            kind = %kind,
            pending = %pending,
            "settlement gate wedged: chain did not settle before the patience backstop",
        );
        SubstrateHarnessError::SettlementTimeout { kind, pending }
    }

    /// Issue 607 Phase 3: spawn an instanced actor onto the harness's
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
        params: A::Params,
    ) -> aether_substrate::SpawnBuilder<'a, A>
    where
        A: Root + Instanced + NativeActor,
    {
        self.passive.spawn_actor::<A>(subname, config, params)
    }

    /// A read-only probe of the chassis's real route table, reached by
    /// `perf::registry`'s benchmark so it measures the published view a
    /// running engine dispatches against.
    pub(crate) fn route_read_probe(&self) -> RouteReadProbe {
        self.passive.route_read_probe()
    }

    /// iamacoffeepot/aether#1057: inject a chassis-root mail and return its
    /// `MailId` plus a settlement [`Receiver`] that fires when the whole
    /// causal tree drains. Unlike [`Self::settle_bytes`] this does NOT
    /// block — the mail-latency harness injects many roots back-to-back
    /// (to build inbox queueing) and waits on the collected receivers
    /// afterward. It pushes through the proof-taking
    /// [`PassiveChassis::send_tracked`], which subscribes settlement before
    /// the push, so a tree that drains at once still fires the receiver.
    #[cfg(test)]
    pub(crate) fn inject_root<K: Kind, I>(
        &self,
        recipient: impl ChassisTarget<K, I>,
        mail: &K,
    ) -> (MailId, Receiver<()>) {
        self.passive.send_tracked(recipient, mail, None)
    }

    /// The lifetime-guard boot, for this crate's `#[cfg(test)]` fixtures,
    /// which take its handles through `SubstrateBoot::handles_for_test`.
    #[cfg(test)]
    const fn boot(&self) -> &SubstrateBoot {
        let Self { _boot: boot, .. } = self;
        boot
    }

    /// ADR-0086 Phase 3: read the chassis-host trace ring — where the
    /// `Sent` for off-actor / injected root mail (e.g. [`Self::inject_root`])
    /// lands, since it's produced outside any actor's stamped slots.
    /// Per-actor rings are queried via `aether.trace.tail` mail; this
    /// ring belongs to no actor, so the test reads it directly.
    #[cfg(test)]
    pub(crate) fn chassis_host_trace_tail(&self, request: &TraceTail) -> TraceTailResult {
        let (_, mailer) = self.boot().handles_for_test();
        mailer.trace_handle().chassis_host_tail(request)
    }

    /// ADR-0086 Phase 3: reconstruct `root`'s trace tree via the
    /// decentralized guided walk over per-actor rings — the in-process
    /// counterpart to the MCP's over-the-wire walk (there is no central
    /// observer post-3c; the rings are the source of truth). Seeds at
    /// `root.sender`
    /// (`CHASSIS_MAILBOX_ID` for an injected root, an actor otherwise),
    /// then fans out across each `Sent`'s recipient. The chassis-host ring
    /// belongs to no actor, so it is read directly (the same ring the
    /// mailer's chassis arm answers `aether.trace.tail` from). Every other
    /// ring is tailed with `aether.trace.tail` through the proof in
    /// `actors` whose position the walk reported; a position no proof in
    /// `actors` names contributes no entries, as an unreachable ring would.
    /// The `root` filter on every tail isolates the tree from the
    /// trace-query traffic itself.
    #[cfg(test)]
    pub(crate) fn describe_tree_walked<R>(&mut self, root: MailId, actors: &[ActorRef<R>]) -> DescribeTreeResult {
        // The in-process harness reaches the substrate's reverse-lookup
        // registry directly, so it resolves each node's thread name
        // (ADR-0102: the resolver is the caller's; the MCP path passes
        // none).
        use aether_substrate::runtime::thread_name;

        let mut walk = TreeWalk::new(root);
        while let Some(mailbox) = walk.next_mailbox() {
            let request = TraceTail { max: 0, since: None, root: Some(root) };
            // A send error or an undecodable reply yields no entries; the
            // walk still completes from the rings that do answer.
            let result = if mailbox == MailboxId::CHASSIS_MAILBOX_ID {
                Some(self.chassis_host_trace_tail(&request))
            } else {
                actors.iter().find(|actor| actor.id() == mailbox).and_then(|actor| {
                    self.request_prepared(&actor.prepare(&request))
                        .ok()
                        .and_then(|reply| TraceTailResult::decode_from_bytes(&reply))
                })
            };
            if let Some(TraceTailResult::Ok { entries, .. }) = result {
                walk.absorb(entries);
            }
        }
        walk.finish_with(|tid| thread_name::resolve(tid.0))
    }

    /// Bytes-level request/reply: push `(kind, payload)` to the actor `to`
    /// proves with this harness's session as the reply target, pump until
    /// the matching reply arrives, and return its raw payload bytes. Backs the `SendAndAwaitReply` op of
    /// [`Self::execute`], where the reply type isn't known statically
    /// and the caller decodes on demand via
    /// [`super::ExecutionResult::reply`]. Used for the
    /// component load/replace/drop round trips and the `aether.fs`
    /// `Read`/`Write`/`Delete`/`List` replies — every standard
    /// `*Result` kind is structured-encoded.
    pub(crate) fn request_prepared(&mut self, send: &PreparedSend) -> Result<Vec<u8>, SubstrateHarnessError> {
        let cid = self.fresh_correlation_id();
        send.for_reply(&self.passive, self.session_reply(cid))
            .map_err(|error| SubstrateHarnessError::Decode(format!("prepare harness request: {error}")))?;
        self.pump_until_reply_bytes(cid, "<await-reply bytes>")
    }

    pub(crate) fn request_bytes<K: Kind, I>(
        &mut self,
        to: impl ChassisTarget<K, I>,
        mail: &K,
    ) -> Result<Vec<u8>, SubstrateHarnessError> {
        let cid = self.fresh_correlation_id();
        self.passive.send_for_reply(to, mail, self.session_reply(cid));
        self.pump_until_reply_bytes(cid, K::NAME)
    }

    /// Enqueue a typed request with this harness's session as the reply target,
    /// but do not pump the chassis or wait for the reply yet.
    ///
    /// This is the asynchronous counterpart to `send_and_await_reply` for tests that
    /// need several requests in flight at once to validate correlation.
    #[must_use]
    pub fn send_deferred<K, I>(&self, to: impl ChassisTarget<K, I>, mail: &K) -> PendingBenchReply
    where
        K: Kind,
    {
        let cid = self.fresh_correlation_id();
        self.passive.send_for_reply(to, mail, self.session_reply(cid));
        PendingBenchReply { cid, expected: K::NAME }
    }

    /// [`Self::send_deferred`] to a reference the harness handed out, such as
    /// a wasm-only fixture's protocol reference from [`Self::cast`].
    ///
    /// # Errors
    ///
    /// [`SubstrateHarnessError::Decode`] when `mail` cannot be prepared for
    /// `to`.
    pub fn send_deferred_to<K: Kind, I>(
        &self,
        to: impl SendTarget<K, I>,
        mail: &K,
    ) -> Result<PendingBenchReply, SubstrateHarnessError> {
        let cid = self.fresh_correlation_id();
        to.prepare(mail)
            .for_reply(&self.passive, self.session_reply(cid))
            .map_err(|error| SubstrateHarnessError::Decode(format!("prepare harness send: {error}")))?;
        Ok(PendingBenchReply { cid, expected: K::NAME })
    }

    /// Pump until the reply for a request returned by [`Self::send_deferred`]
    /// arrives, stashing out-of-order replies for later awaits.
    pub fn await_deferred<R>(&mut self, pending: PendingBenchReply) -> Result<R, SubstrateHarnessError>
    where
        R: Kind,
    {
        self.pump_until_reply(pending.cid, pending.expected)
    }

    /// Push `mail` to `to` as a tracked chassis root and return the root,
    /// without pumping anything or waiting for it to settle. The push lands
    /// in the recipient's inbox before this returns, so it is ordered ahead
    /// of anything the harness drives afterwards.
    ///
    /// # Errors
    ///
    /// [`SubstrateHarnessError::Decode`] when the send cannot be prepared.
    pub fn send_tracked<K: Kind, I>(
        &self,
        to: impl SendTarget<K, I>,
        mail: &K,
    ) -> Result<MailId, SubstrateHarnessError> {
        to.prepare(mail)
            .tracked(&self.passive, None)
            .map(|(root, _)| root)
            .map_err(|error| SubstrateHarnessError::Decode(format!("prepare harness send: {error}")))
    }

    /// Block, without dispatching anything, until a mail of kind `K` is
    /// queued for the pumped component host, waiting on its mailbox wake.
    /// Nothing the host would do in reply to its queue happens meanwhile, so
    /// every mail queued ahead of that one arrived while the host held
    /// still; [`Self::step_component_host_through`] then dispatches exactly
    /// those, and that one.
    ///
    /// # Errors
    ///
    /// [`SubstrateHarnessError::SettlementTimeout`] when no such mail is
    /// queued within the settlement cap.
    ///
    /// # Panics
    ///
    /// Panics when the harness was built without
    /// [`SubstrateHarnessBuilder::with_pumped_component_host`].
    pub fn await_component_host_queued<K: Kind>(&mut self) -> Result<(), SubstrateHarnessError> {
        let gate = "substrate_harness.await_component_host_queued";
        let start = Instant::now();
        loop {
            let PumpedHost(slot) = self.component_host.as_mut().expect("the harness composed a pumped component host");
            if slot.queued_kinds().contains(&K::ID) {
                return Ok(());
            }
            match self.wake_rx.recv_timeout(SETTLEMENT_TIMEOUT) {
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) if start.elapsed() < self.settlement_cap => tracing::warn!(
                    target: "aether_substrate::substrate_harness",
                    gate,
                    waited_millis = start.elapsed().as_millis(),
                    "component host queue slow: still waiting for {}, extending",
                    K::NAME,
                ),
                Err(_) => return Err(self.settlement_timeout(K::NAME.to_owned(), gate)),
            }
        }
    }

    /// Run the pumped component host one envelope at a time until it has
    /// dispatched `count` mails of kind `K`, waiting on its mailbox wake
    /// whenever its inbox is empty. It stops right after the last one, so
    /// whatever that turn staged has not yet come back to it: after the
    /// host dispatched a republish member's `Prepared`, that member is
    /// prepared and its commit has not been sent.
    ///
    /// # Errors
    ///
    /// [`SubstrateHarnessError::SettlementTimeout`] when the host has not
    /// dispatched them within the settlement cap.
    ///
    /// # Panics
    ///
    /// Panics when the harness was built without
    /// [`SubstrateHarnessBuilder::with_pumped_component_host`].
    pub fn step_component_host_through<K: Kind>(&mut self, count: usize) -> Result<(), SubstrateHarnessError> {
        let gate = "substrate_harness.step_component_host";
        let start = Instant::now();
        let mut seen = 0;
        while seen < count {
            let PumpedHost(slot) = self.component_host.as_mut().expect("the harness composed a pumped component host");
            match slot.dispatch_one() {
                Some(kind) => seen += usize::from(kind == K::ID),
                None => match self.wake_rx.recv_timeout(SETTLEMENT_TIMEOUT) {
                    Ok(_) => {}
                    Err(RecvTimeoutError::Timeout) if start.elapsed() < self.settlement_cap => tracing::warn!(
                        target: "aether_substrate::substrate_harness",
                        gate,
                        waited_millis = start.elapsed().as_millis(),
                        "component host step slow: still waiting for {}, extending",
                        K::NAME,
                    ),
                    Err(_) => return Err(self.settlement_timeout(K::NAME.to_owned(), gate)),
                },
            }
        }
        Ok(())
    }

    /// Run `ticks` complete frames synchronously. Each frame
    /// dispatches `Tick` to subscribers, drains the queue, and
    /// renders. Returns once the substrate has replied with
    /// `AdvanceResult::Ok`.
    pub(crate) fn advance(&mut self, ticks: u32, delta_micros: u32) -> Result<u32, SubstrateHarnessError> {
        let cid = self.fresh_correlation_id();
        // Issue 603 Phase 4: advance migrated from `aether.control`
        // (chassis_handler closure) onto `aether.substrate_harness`
        // (`SubstrateHarnessCapability`).
        self.passive.send_for_reply(
            self.passive.actor_ref::<SubstrateHarnessCapability>(),
            &Advance { ticks, delta_micros },
            self.session_reply(cid),
        );
        match self.pump_until_reply::<AdvanceResult>(cid, "AdvanceResult")? {
            AdvanceResult::Ok { ticks_completed } => Ok(ticks_completed),
            AdvanceResult::Err { error } => Err(SubstrateHarnessError::Advance(error)),
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
    pub(crate) fn capture(&mut self) -> Result<Vec<u8>, SubstrateHarnessError> {
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
        pre: Vec<aether_kinds::NamedMail>,
        after: Vec<aether_kinds::NamedMail>,
    ) -> Result<Vec<u8>, SubstrateHarnessError> {
        // ADR-0161 slice R4: capture_frame routes to the pumped render
        // actor; the hook supplies its reference so the core stays
        // render-free. No hook ⇒ no render slot booted ⇒ fail fast instead
        // of warn-dropping the mail. The pumped `on_capture_frame` parks the
        // request; `pump_until_reply` drives the slot (drain + frame) until
        // the deferred reply lands on the loopback.
        let render =
            self.hook.as_ref().map(|hook| hook.render()).ok_or_else(|| {
                SubstrateHarnessError::Capture("render not composed — no capture pipeline".to_owned())
            })?;
        let cid = self.fresh_correlation_id();
        let mail = CaptureFrame {
            window: None,
            mails: pre,
            after_mails: after,
            // The `SubstrateHarness::capture` API returns the PNG only; the
            // substrate-side verdict path (iamacoffeepot/aether#1777)
            // and similarity path (iamacoffeepot/aether#1780) are
            // exercised through `HarnessOp::send_and_await_reply` scenarios.
            checks: Vec::new(),
            similarity: None,
        };
        (&render)
            .prepare(&mail)
            .for_reply(&self.passive, self.session_reply(cid))
            .map_err(|error| SubstrateHarnessError::Capture(format!("prepare capture request: {error}")))?;
        match self.pump_until_reply::<CaptureFrameResult>(cid, "CaptureFrameResult")? {
            CaptureFrameResult::Ok { png, .. } => Ok(png),
            CaptureFrameResult::Err { error } => Err(SubstrateHarnessError::Capture(error)),
        }
    }

    /// This harness's session as the reply target for correlation `cid`.
    /// Issue 603 retired `aether.control` as the catch-all for
    /// chassis-peripheral kinds; each one now routes to its own cap
    /// (`aether.render.capture_frame`, `aether.substrate_harness.advance`,
    /// `aether.window.set_mode`, etc.) and replies to the session here.
    pub(crate) const fn session_reply(&self, cid: u64) -> ReplyTarget {
        ReplyTarget::Session { session: self.session, correlation: cid }
    }

    /// The next session-reply correlation: the `ReplyTarget::Session`
    /// correlation a recipient echoes back, which `pump_until_event` matches
    /// the loopback reply by. It names no root — `send_tracked` mints those
    /// from the engine's one chassis-root counter.
    pub(crate) fn fresh_correlation_id(&self) -> u64 {
        // 0 is the "no correlation" sentinel so skip it.
        let id = self.next_correlation_id.fetch_add(1, Ordering::SeqCst);
        if id == 0 {
            self.next_correlation_id.fetch_add(1, Ordering::SeqCst)
        } else {
            id
        }
    }

    /// Pump the event channel, the render slot and the loopback receiver
    /// until a reply with `cid` arrives, decoded as `R`.
    fn pump_until_reply<R>(&mut self, cid: u64, expected: &'static str) -> Result<R, SubstrateHarnessError>
    where
        R: Kind,
    {
        let event = self.pump_until_event(cid, expected)?;
        Self::decode_reply::<R>(event, expected)
    }

    /// Pump until the reply with `cid` arrives, returning the raw
    /// reply payload bytes instead of decoding. Backs
    /// [`Self::request_bytes`] and the `SendAndAwaitReply` op of
    /// [`Self::execute`], where the reply type is decoded on demand.
    fn pump_until_reply_bytes(&mut self, cid: u64, expected: &'static str) -> Result<Vec<u8>, SubstrateHarnessError> {
        let event = self.pump_until_event(cid, expected)?;
        Self::reply_payload(event, expected)
    }

    /// Pump the event channel, the render slot and the loopback receiver
    /// until a session-targeted reply with `cid` arrives, returning the raw
    /// [`EgressEvent`]. Shared loop body of [`Self::pump_until_reply`]
    /// (typed decode) and [`Self::pump_until_reply_bytes`] (raw bytes).
    ///
    /// A quiet iteration blocks on the one wake channel every source fires
    /// after it enqueues, so the loop wakes when a source has work, never on
    /// a clock. No wake is lost because this loop alone empties the queue
    /// inside a pump wait, and always before it drains the sources: a wake
    /// emptied here was fired after its item was enqueued, so the drain that
    /// follows finds the item; an item enqueued after the empty fires its
    /// wake after the empty too, so that wake stays queued and the block
    /// below returns on it. A render settle ([`FrameHook::settle`]) also
    /// reads the channel, but only between pump waits, and each pump wait
    /// drains every source before it first blocks, so a wake that settle
    /// consumed is covered.
    pub(crate) fn pump_until_event(
        &mut self,
        cid: u64,
        expected: &'static str,
    ) -> Result<EgressEvent, SubstrateHarnessError> {
        // Wall-clock budget for consecutive quiet (no-progress) time
        // before giving up — a deadlock/livelock backstop, not the gate a
        // healthy reply meets, so it reads the runtime-configurable
        // settlement cap (issue 2062) rather than a 1-min constant that
        // false-fired under nextest saturation. The block below waits at
        // most the budget left, so this bounds a failure and never paces a
        // poll. The default 5 min rides out a wasmtime compile under
        // parallel-test CPU pressure (issue 603 routed `LoadComponent`
        // through `ComponentHostCapability`'s thread, so a load step waits
        // on a dispatcher hop + compile when N test binaries run in
        // parallel); `Duration::MAX` (the no-cap sentinel) waits forever.
        let stall_deadline = self.settlement_cap;

        let mut last_progress = Instant::now();
        let mut iterations = 0u32;
        loop {
            iterations = iterations.saturating_add(1);

            // Empty the wake queue before draining any source — the order the
            // no-lost-wake argument above rests on. No other path empties it
            // inside a pump wait.
            while self.wake_rx.try_recv().is_ok() {}

            // A reply a nested pump (an advance's per-tick wait, run from
            // `dispatch_event` below) read off the loopback for this `cid`.
            if let Some(frame) = self.stashed_replies.remove(&cid) {
                return Ok(frame);
            }

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

            // ADR-0161 slice R4: the harness is the pumped render actor's
            // driver, so it must drain the slot on every pump iteration —
            // render-recipient chains (an advance's `DrawTriangle` /
            // `ViewProjection`, a capture's issue-860 pre-mails and their
            // `pre_settled` notices) settle only because this pump runs, the
            // deadlock the ADR names.
            //
            // The capture ordering barrier (the stale-frame race the reverted
            // #3923 hit): drive the capture frame only once the parked capture
            // is *ready* — every pre-mail chain settled (`pre_remaining == 0`),
            // so the draws those chains terminate at (issue 860) have already
            // dispatched onto the render accumulators. A chain cannot settle
            // until its terminal render handler ran, and `pump()` above is what
            // drains those handlers, so `capture_ready()` going true *is* the
            // pre-mail chains having settled with the slot drained. Sending the
            // frame earlier — on a merely-pending capture, as the reverted
            // attempt did — could record `replay_cache_when_idle: true` against
            // an empty accumulator before the tick's draw landed, replaying the
            // prior frame's cache. Once ready, exactly one frame (issue 847
            // replay-cache) records the accumulators and the actor's deferred
            // reply lands on the loopback below.
            if let Some(hook) = self.hook.as_mut() {
                hook.pump();
                if hook.capture_ready() {
                    hook.send_frame(/* replay_cache_when_idle */ true);
                    progressed = true;
                }
            }
            // The harness is the pumped component host's driver too, so a
            // reply that needs the host's turns gets them here.
            if let Some(PumpedHost(slot)) = self.component_host.as_mut() {
                slot.drain_available();
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
                last_progress = Instant::now();
                continue;
            }

            let remaining = stall_deadline.saturating_sub(last_progress.elapsed());
            if remaining.is_zero() {
                return Err(SubstrateHarnessError::Timeout { expected, pumped_iterations: iterations });
            }
            let woke = if stall_deadline == Duration::MAX {
                self.wake_rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
            } else {
                self.wake_rx.recv_timeout(remaining)
            };
            // A wake loops back to empty the rest of the queue and drain; a
            // timeout loops back to the budget check above, which reports it.
            // A disconnect means every source's wake is gone, so no reply can
            // arrive.
            if woke == Err(RecvTimeoutError::Disconnected) {
                return Err(SubstrateHarnessError::Timeout { expected, pumped_iterations: iterations });
            }
        }
    }

    fn decode_reply<R>(event: EgressEvent, expected: &'static str) -> Result<R, SubstrateHarnessError>
    where
        R: Kind,
    {
        match event {
            EgressEvent::ToSession { kind_name, payload, .. } => {
                // ADR-0100: decode through the kind's declared codec
                // (cast or structured), not a hardcoded structured path.
                R::decode_from_bytes(&payload).ok_or_else(|| {
                    SubstrateHarnessError::Decode(format!(
                        "{expected} decode failed via Kind::decode_from_bytes (kind={kind_name})"
                    ))
                })
            }
            other => Err(SubstrateHarnessError::Decode(format!("expected {expected} reply event, got {other:?}"))),
        }
    }

    /// Extract the raw payload bytes from a session-targeted reply
    /// event. The bytes-level counterpart to [`Self::decode_reply`] —
    /// the caller decodes later via [`super::ExecutionResult::reply`].
    fn reply_payload(event: EgressEvent, expected: &'static str) -> Result<Vec<u8>, SubstrateHarnessError> {
        match event {
            EgressEvent::ToSession { payload, .. } => Ok(payload),
            other => Err(SubstrateHarnessError::Decode(format!("expected {expected} reply event, got {other:?}"))),
        }
    }

    /// Run one chassis event. Runs inline on the test thread.
    ///
    /// Returns the error `run_frame`'s per-tick advance produces if the
    /// chain never settles: a `Timeout` waiting on the driver's
    /// `LifecycleAdvanceComplete` reply (the broadcast subtree leaked an
    /// `in_flight`, or the driver never replied), or a `SettlementTimeout`
    /// from a capture pre-mail chain. In the Advance branch we bail
    /// mid-loop without sending `AdvanceResult::Ok`: the carried inbound
    /// guard drops unreplied, which is ordinary for an `InboundMail`, and
    /// the `pump_until_reply` caller surfaces the timeout rather than
    /// waiting on a reply that will never arrive — the substrate is
    /// in a stuck state and the test should fail loudly. On success the
    /// reply goes through that guard, which answers every sender kind and
    /// holds the request's chain open until the ticks complete.
    fn dispatch_event(&mut self, event: ChassisEvent) -> Result<(), SubstrateHarnessError> {
        let ChassisEvent::Advance { reply, ticks, delta_micros } = event;

        for _ in 0..ticks {
            self.frame += 1;
            self.run_frame(delta_micros)?;
        }
        reply.reply(&AdvanceResult::Ok { ticks_completed: ticks });

        Ok(())
    }

    fn run_frame(&mut self, delta_micros: u32) -> Result<(), SubstrateHarnessError> {
        // ADR-0082 PR 3b: SubstrateHarness pushes `LifecycleAdvance` to the
        // lifecycle driver, which broadcasts the `Tick` stage directly
        // to its stage subscribers (components subscribe `Tick` on
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
        // Waiting on the raw settlement channel woke the harness (and
        // let it push the next tick's advance) *before* the driver
        // had cleared `pending`, so under parallel-nextest load the
        // next advance hit `pending.is_some()` and warn-dropped one
        // tick (199 broadcasts, not 200). Correlating on the
        // `LifecycleAdvanceComplete` reply — emitted strictly after
        // `pending` clears — closes that race: by the time the reply
        // lands, the driver is ready for the next advance and the
        // whole broadcast subtree has settled (the reply is gated on
        // settlement).
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
            // Push a chassis-root `LifecycleAdvance` (so the trace pipeline
            // tracks the broadcast subtree and `on_settled` fires) that *also*
            // carries this harness's session as the reply target — the driver
            // routes `LifecycleAdvanceComplete` there via `on_settled`'s
            // `ctx.reply_to`. The reply is the gate, so the settlement
            // receiver goes unread.
            let _ = self.passive.send_tracked(
                self.lifecycle,
                &aether_kinds::LifecycleAdvance { delta_micros },
                Some(self.session_reply(cid)),
            );
            // Block until the driver replies `LifecycleAdvanceComplete`
            // for this advance. A `Timeout` here means the chain never
            // settled (a genuine in_flight leak in some downstream cap)
            // or the driver never replied — same fail-loud disposition
            // the prior `SettlementTimeout` had.
            let complete =
                self.pump_until_reply::<aether_kinds::LifecycleAdvanceComplete>(cid, "LifecycleAdvanceComplete")?;
            if complete.next == <Tick as Kind>::ID.0 || complete.next == 0 {
                break;
            }
        }
        // ADR-0082 §6 / PR 3c: the advance settlement above already waited
        // for the whole frame chain (Tick → component → DrawTriangle →
        // pumped render accumulator) to drain — and because the pumped
        // render slot is drained inside `pump_until_event`, the advance's
        // `DrawTriangle` / `ViewProjection` handlers have already dispatched
        // onto the owned accumulators by the time we reach here.
        //
        // ADR-0161 slice R4: record the frame by mailing one
        // `aether.render.frame { replay_cache_when_idle: false }` (advance
        // commits current) and draining the slot. Capture is no longer a
        // driver-side readback — the pumped `on_capture_frame` owns the
        // mail-driven capture machine, driven from `pump_until_event`.
        if let Some(hook) = self.hook.as_mut() {
            hook.send_frame(/* replay_cache_when_idle */ false);
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
        .map(|(root, in_flight, held_open)| format!("{root:?} → in_flight={in_flight} held_open={held_open}"))
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
        let a = MailId { sender: MailboxId(1), correlation_id: 2 };
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

    use crate::HarnessOp;

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
        let tb = match SubstrateHarness::start_with_size(64, 48) {
            Ok(tb) => tb,
            Err(e) => {
                eprintln!("skipping: SubstrateHarness boot failed (likely no wgpu adapter): {e}");
                return;
            }
        };
        // A synthetic root that never settles: one `Sent`, no `Finished`.
        let stuck = MailId { sender: MailboxId(0xDEAD), correlation_id: 0xBEEF };
        let (_, mailer) = tb.boot().handles_for_test();
        mailer.trace_handle().settlement_counter().record_sent(stuck);

        let err = tb.settlement_timeout("StuckKind".to_owned(), "test.wedge");
        let SubstrateHarnessError::SettlementTimeout { pending, .. } = &err else {
            panic!("expected SettlementTimeout, got {err:?}");
        };
        assert!(pending.contains("in_flight=1"), "dump should name the stuck root with in_flight=1: {pending}");
        // The rendered error string carries the dump too.
        assert!(err.to_string().contains("in_flight=1"), "Display should surface the pending dump: {err}");
    }

    /// The window capability's listing row, as a test names a fixture it
    /// cannot type.
    #[aether_actor::protocol]
    trait WindowLister {
        fn list(mail: aether_window::ListWindows) -> aether_window::ListWindowsResult;
    }

    /// A row the window capability does not publish.
    #[aether_actor::protocol]
    trait Pinger {
        fn ping(mail: aether_kinds::Ping) -> aether_kinds::Pong;
    }

    /// `cast` types an erased reference only when the route's published rows
    /// answer the protocol, and a send through the cast reference reaches the
    /// actor: the window capability publishes `ListWindows -> ListWindowsResult`,
    /// so the cast mints and its reply decodes, while `Ping -> Pong` is refused
    /// with the protocol named.
    #[test]
    fn cast_types_an_erased_reference_by_its_published_rows() {
        use aether_window::{ListWindows, ListWindowsResult, WindowCapability};

        let mut harness = SubstrateHarness::start().expect("boot harness");
        let window = harness.actor_ref::<WindowCapability>().erase();

        let listed = harness.cast::<WindowLister>(window).expect("the window publishes the listing row");
        let result = harness
            .execute(vec![("list", HarnessOp::send_and_await_reply(&listed, &ListWindows))])
            .expect("the listing round trip completes");
        assert_eq!(
            result.reply::<ListWindowsResult>("list").expect("the reply decodes"),
            ListWindowsResult::Ok { windows: Vec::new() },
        );

        let Err(refused) = harness.cast::<Pinger>(window) else {
            panic!("the window publishes no Ping row");
        };
        let SubstrateHarnessError::CastRefused { protocol, path } = &refused else {
            panic!("expected CastRefused, got {refused:?}");
        };
        assert_eq!(*protocol, type_name::<Pinger>());
        assert!(path.is_some(), "a composed capability retains its path");
    }

    use aether_substrate::{BootError, NativeCtx, NativeInitCtx};

    /// A lifecycle stage subscriber for the scenarios below: silent `Tick`
    /// and `Shutdown` handlers, so its path narrows to a subscriber of each,
    /// that forward what they receive to the harness observer, where
    /// `count_observed` counts it and the forward's own trace carries the
    /// broadcast's lineage.
    struct StageRelay;

    #[aether_actor::actor(singleton, root, depends(aether_test_fixtures_kinds::SubstrateHarnessObserver))]
    impl NativeActor for StageRelay {
        const NAMESPACE: &'static str = "test.harness.stage_relay";
        type Config = ();

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }

        #[handler::event]
        fn on_tick(&mut self, ctx: &mut NativeCtx<'_>, tick: Tick) {
            let _ = self;
            ctx.send::<aether_test_fixtures_kinds::SubstrateHarnessObserver>(&tick);
        }

        #[handler::event]
        fn on_shutdown(&mut self, ctx: &mut NativeCtx<'_>, shutdown: aether_kinds::Shutdown) {
            let _ = self;
            ctx.send::<aether_test_fixtures_kinds::SubstrateHarnessObserver>(&shutdown);
        }
    }

    /// Boot a harness composing the [`StageRelay`], or `None` when no wgpu
    /// adapter is available.
    fn stage_relay_harness() -> Option<SubstrateHarness> {
        match SubstrateHarness::builder().size(64, 48).with_actor::<StageRelay>(()).build() {
            Ok(tb) => Some(tb),
            Err(e) => {
                eprintln!("skipping: SubstrateHarness boot failed (likely no wgpu adapter): {e}");
                None
            }
        }
    }

    /// Subscribe the [`StageRelay`] to one lifecycle stage by its path.
    fn subscribe_relay(tb: &SubstrateHarness, subscription: aether_lifecycle::LifecycleSubscription) -> HarnessOp {
        HarnessOp::send_and_settle(
            &tb.actor_ref::<aether_lifecycle::LifecycleCapability>(),
            &aether_lifecycle::LifecycleSubscribe { subscription },
        )
    }

    /// Issue iamacoffeepot/aether#723: chassis-source ticks are minted
    /// as chassis roots, and the lifecycle cap fanout
    /// propagates `(root, parent_mail)` from the inbound through
    /// `NativeCtx::fanout` so each subscriber-bound copy lands in the
    /// same causal chain. Verified by subscribing the [`StageRelay`] to
    /// ticks, advancing one tick, and reading the relay's forward of the
    /// `Tick` it received from its own trace ring: the forward's parent is
    /// the fanned-out copy, and its root is the advance's, not that copy.
    #[test]
    fn tick_fanout_propagates_chassis_root_lineage() {
        use aether_actor::ActorPath;
        use aether_data::Kind as DataKind;
        use aether_kinds::trace::{TraceEvent, TraceTail, TraceTailResult};
        use aether_lifecycle::LifecycleSubscription;

        let Some(mut tb) = stage_relay_harness() else {
            return;
        };

        // Subscribe the relay to the `Tick` lifecycle stage (goes through the
        // lifecycle cap's on_subscribe handler), then advance one tick. The
        // advance issues a `LifecycleAdvance` whose chain root the lifecycle
        // cap threads through `broadcast_to_subscribers` (`NativeCtx::fanout`)
        // to every stage subscriber (issue 723 lineage, ADR-0082 §6).
        // Sequencing both through `execute` settles the subscribe before the
        // tick fires.
        let subscription = LifecycleSubscription::Tick(ActorPath::<StageRelay>::root().narrow());
        tb.execute(vec![("subscribe", subscribe_relay(&tb, subscription)), ("advance", HarnessOp::advance(1))])
            .expect("subscribe + advance");
        assert!(tb.count_observed(Tick::NAME) > 0, "subscriber received no Tick — fanout never reached it");

        let reply = tb
            .request_prepared(&(&tb.actor_ref::<StageRelay>()).prepare(&TraceTail { max: 0, since: None, root: None }))
            .expect("the relay's ring answers");
        let Some(TraceTailResult::Ok { entries, .. }) = TraceTailResult::decode_from_bytes(&reply) else {
            panic!("the relay's trace tail decodes");
        };
        let (mail_id, root, parent) = entries
            .iter()
            .find_map(|entry| match entry.event {
                TraceEvent::Sent { mail_id, root, parent_mail, kind, .. } if kind == Tick::ID => {
                    Some((mail_id, root, parent_mail))
                }
                _ => None,
            })
            .expect("the relay's ring records the forward of the Tick it received");
        let parent = parent.expect("the forward carries the fanned-out Tick copy as its parent");
        // Issue 723 fix: each fanned-out copy gets its own MailId, but the
        // root is inherited from the chassis-root tick. Pre-fix the copy
        // was orphaned and rooted its own chain, so the forward's root would
        // be the copy it answers.
        assert_ne!(
            root, parent,
            "the fanned-out copy should inherit the advance's root rather than root its own chain"
        );
        assert_ne!(mail_id, parent, "the forward is a child node in the trace tree, not its parent");
    }

    /// iamacoffeepot/aether#1489: a `Quit` mail drives the frame
    /// lifecycle to its `Shutdown` terminal, finishing the in-flight
    /// `Tick → Render → Present` frame first because the quit edge lives
    /// on `Present` (ADR-0082 §3). This is the CI-runnable coverage for
    /// the drain — the desktop winit `CloseRequested` / ctrlc bridges
    /// that push the `Quit` are MCP-smoke territory, but the `Quit →
    /// Present → Shutdown` graph behaviour they depend on is exercised
    /// here without a live window (the harness shares the same
    /// `frame_lifecycle_config` graph desktop uses).
    ///
    /// Subscribes the [`StageRelay`] to the `Shutdown` stage, sends `Quit`
    /// to `aether.lifecycle`, then advances one frame. The run-frame loop
    /// drives the whole `Tick → Render → Present → Shutdown` chain in that
    /// single advance once `quit_pending` is set (each stage breaks the
    /// loop only at `Tick` cycle-complete or the `next == 0` terminal), so
    /// observing the relay's forward of the `Shutdown` broadcast proves the
    /// quit was consumed at `Present` and that `Shutdown` fired + settled.
    #[test]
    fn quit_drains_frame_then_broadcasts_shutdown() {
        use aether_actor::ActorPath;
        use aether_data::Kind as DataKind;
        use aether_kinds::{Quit, Shutdown};
        use aether_lifecycle::LifecycleSubscription;

        let Some(mut tb) = stage_relay_harness() else {
            return;
        };

        // Subscribe to Shutdown, set the quit flag, then advance one
        // frame — `execute` settles each step before the next, so the
        // subscription and `quit_pending` are both in place when the
        // advance fires.
        let subscription = LifecycleSubscription::Shutdown(ActorPath::<StageRelay>::root().narrow());
        tb.execute(vec![
            ("subscribe_shutdown", subscribe_relay(&tb, subscription)),
            ("quit", HarnessOp::send_and_settle(&tb.actor_ref::<aether_lifecycle::LifecycleCapability>(), &Quit {})),
            ("advance", HarnessOp::advance(1)),
        ])
        .expect("subscribe + quit + advance");

        assert!(
            tb.count_observed(Shutdown::NAME) > 0,
            "Shutdown broadcast never reached the subscriber — quit was not drained to the terminal; observed \
             kinds: {:?}",
            tb.observed_kinds(),
        );
    }

    /// Issue 607 Phase 3 verify: spawn an instanced actor through
    /// `SubstrateHarness::spawn_actor`, exercise `Subname::Counter` +
    /// `Subname::Named`, assert each returned proof names a live slot,
    /// confirm reused subnames fail, and confirm `after_init` mail lands
    /// as the actor's first dispatch.
    #[test]
    fn spawn_instanced_actor_smoke() {
        use aether_actor::{Addressable as ActorTrait, HandlesKind};
        use aether_data::{Kind as DataKind, KindId as DataKindId};
        use aether_substrate::{BootError, Dispatch, NativeActor, NativeCtx, NativeInitCtx, SpawnError, Subname};
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
        impl aether_data::ActorMail for Bump {}
        impl aether_data::CrossesActors for Bump {}

        struct Child {
            received: Arc<AtomicU32>,
        }
        impl ActorTrait for Child {
            const NAMESPACE: &'static str = "test.spawn.child";
            type Resolver = aether_actor::Many;
        }
        impl Root for Child {}
        impl HandlesKind<Bump> for Child { type Sender = aether_actor::Anyone; }
        impl aether_actor::Lifecycle<Self> for Child {
            type Config = Arc<AtomicU32>;
            type Params = ();
            type InitError = BootError;
            type InitCtx<'a> = NativeInitCtx<'a>;
            type Ctx<'a> = NativeCtx<'a, Self>;
            fn init(config: Self::Config, _params: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self { received: config })
            }
        }
        impl aether_actor::Declared for Child {
            type Depends = ();
            type Spawns = ();
            type Parents = ();
        }
        impl NativeActor for Child {
            type State = Self;
        }
        impl Dispatch<Self> for Child {
            fn dispatch(
                state: &mut Self,
                _ctx: &mut NativeCtx<'_, Self, aether_substrate::Unchecked>,
                kind: KindId,
                payload: &[u8],
            ) -> Option<()> {
                if kind.0 == Bump::ID.0 {
                    let _ = Bump::decode_from_bytes(payload)?;
                    state.received.fetch_add(1, AtomicOrdering::SeqCst);
                    return Some(());
                }
                None
            }
        }

        let mut tb = match SubstrateHarness::start_with_size(64, 48) {
            Ok(tb) => tb,
            Err(e) => {
                eprintln!("skipping: SubstrateHarness boot failed (likely no wgpu adapter): {e}");
                return;
            }
        };

        let received = Arc::new(AtomicU32::new(0));

        // Subname::Counter — first instance, full name "test.spawn.child:0".
        let first = tb
            .spawn_actor::<Child>(Subname::Counter, Arc::clone(&received), ())
            .after_init(Bump { tag: 1 })
            .after_init(Bump { tag: 2 })
            .finish()
            .expect("first counter spawn");

        // Subname::Named — second instance, full name "test.spawn.child:alpha".
        tb.spawn_actor::<Child>(Subname::Named("alpha"), Arc::clone(&received), ()).finish().expect("named spawn");

        // Reused subname → SubnameInUse.
        let err = tb
            .spawn_actor::<Child>(Subname::Named("alpha"), Arc::clone(&received), ())
            .finish()
            .expect_err("reused subname must fail");
        assert!(matches!(err, SpawnError::SubnameInUse { .. }), "expected SubnameInUse, got {err:?}");

        // Barrier: the `after_init` mails are queued on the first instance
        // ahead of any later mail, so a tracked send to it settles only after
        // both of them dispatched.
        tb.settle_bytes(first, &Bump { tag: 3 }).expect("barrier bump settles");
        assert_eq!(
            received.load(AtomicOrdering::SeqCst),
            3,
            "both pre-loaded after_init mails should dispatch to the first instance ahead of the barrier"
        );
    }
}
